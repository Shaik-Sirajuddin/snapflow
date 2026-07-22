//! `snapflowd-mcp`: a minimal, real MCP (Model Context Protocol)
//! server, spoken to over stdio, exposing panel-rust's discovered skills
//! (`skills_state::scan_skills_dir`) as tools an ACP-side agent can call
//! mid-session: `list_skills`, `read_skill`, `list_skill_files`, and
//! `read_skill_file`.
//!
//! `skill_injection_verification` phase (skills-settings-e2e-verification
//! plan): traced through acpx-core/src/gateway_actor/thread_actor.rs and
//! found `session/new`/`session/load` already accept a client-supplied
//! `mcpServers` array (previously always sent as `[]`) -- this binary is
//! what `agent_bridge.rs` now points that array at, so every session gets
//! real, live skill-list access without needing any ACPX profile
//! association (a separate, profile.mcp_servers-based mechanism that
//! would have required a policy decision this binary deliberately avoids
//! needing at all).
//!
//! Deliberately hand-rolled (no MCP SDK dependency) -- the wire surface
//! needed is exactly 3 methods (`initialize`, `tools/list`, `tools/call`)
//! plus the `notifications/initialized` no-op, newline-delimited JSON-RPC
//! 2.0 over stdio, matching MCP's real transport. Not a general-purpose
//! MCP server implementation; scoped to what this one tool surface needs.

use panel_rust::skills_state::{
    merge_skills_for_context, project_skills_dir, scan_skills_dir, SkillEntry, SkillScope,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};

const MAX_SKILL_FILE_BYTES: u64 = 512 * 1024;
const MAX_SKILL_FILES: usize = 512;

struct Args {
    global_dir: PathBuf,
    project_dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut global_dir = None;
    let mut project_dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--global-dir" => global_dir = args.next().map(PathBuf::from),
            "--project-dir" => project_dir = args.next().map(PathBuf::from),
            _ => {}
        }
    }
    Args {
        global_dir: global_dir.expect("--global-dir is required"),
        project_dir,
    }
}

fn context_skills(args: &Args) -> Vec<SkillEntry> {
    let global_skills = scan_skills_dir(&args.global_dir, SkillScope::Global);
    let project_skills = args
        .project_dir
        .as_ref()
        .map(|project_dir| {
            scan_skills_dir(&project_skills_dir(project_dir), SkillScope::Project)
        })
        .unwrap_or_default();
    merge_skills_for_context(global_skills, project_skills)
}

fn skill_by_name(args: &Args, name: &str) -> Result<SkillEntry, String> {
    context_skills(args)
        .into_iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| format!("no skill named {name:?} found"))
}

fn list_skills_result(args: &Args) -> Value {
    let entries = context_skills(args);
    let skills: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "name": e.name,
                "description": e.description,
                "scope": e.scope.as_str(),
                "startedFrom": e.started_from,
            })
        })
        .collect();
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string(&skills).expect("skills list serializes"),
        }]
    })
}

fn read_skill_result(args: &Args, name: &str) -> Value {
    let Ok(entry) = skill_by_name(args, name) else {
        return json!({
            "content": [{"type": "text", "text": format!("no skill named {name:?} found")}],
            "isError": true,
        });
    };
    let skill_md = entry.path.join("SKILL.md");
    match std::fs::read_to_string(&skill_md) {
        Ok(content) => json!({"content": [{"type": "text", "text": content}]}),
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("failed to read {skill_md:?}: {error}")}],
            "isError": true,
        }),
    }
}

/// Returns a regular file below `skill_dir` only when every path component
/// is relative and non-symlinked. Skills are user-authored, but the MCP
/// server must still not become a path-traversal escape hatch.
fn resolve_skill_file(skill_dir: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty() {
        return Err("path is required".to_string());
    }

    let mut resolved = skill_dir.to_path_buf();
    let mut saw_component = false;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("path must be a relative file path without traversal".to_string());
        };
        saw_component = true;
        resolved.push(part);
        let metadata = std::fs::symlink_metadata(&resolved)
            .map_err(|error| format!("failed to access {relative_path:?}: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("symbolic links are not available through skills".to_string());
        }
    }
    if !saw_component {
        return Err("path is required".to_string());
    }
    if !std::fs::metadata(&resolved)
        .map_err(|error| format!("failed to access {relative_path:?}: {error}"))?
        .is_file()
    {
        return Err("path must name a regular file".to_string());
    }
    Ok(resolved)
}

fn collect_skill_files(
    root: &Path,
    relative: &Path,
    files: &mut Vec<String>,
) -> std::io::Result<()> {
    if files.len() >= MAX_SKILL_FILES {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // Ignore links rather than following them outside the skill folder.
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let child_relative = relative.join(entry.file_name());
        if file_type.is_dir() {
            collect_skill_files(&path, &child_relative, files)?;
        } else if file_type.is_file() && child_relative != Path::new("SKILL.md") {
            files.push(child_relative.to_string_lossy().into_owned());
            if files.len() >= MAX_SKILL_FILES {
                break;
            }
        }
    }
    Ok(())
}

fn list_skill_files_result(args: &Args, name: &str) -> Value {
    let Ok(entry) = skill_by_name(args, name) else {
        return json!({
            "content": [{"type": "text", "text": format!("no skill named {name:?} found")}],
            "isError": true,
        });
    };
    let mut files = Vec::new();
    match collect_skill_files(&entry.path, Path::new(""), &mut files) {
        Ok(()) => {
            files.sort();
            json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&files).expect("skill file list serializes"),
                }]
            })
        }
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("failed to list skill files: {error}")}],
            "isError": true,
        }),
    }
}

fn read_skill_file_result(args: &Args, name: &str, path: &str) -> Value {
    let Ok(entry) = skill_by_name(args, name) else {
        return json!({
            "content": [{"type": "text", "text": format!("no skill named {name:?} found")}],
            "isError": true,
        });
    };
    let file = match resolve_skill_file(&entry.path, path) {
        Ok(file) => file,
        Err(error) => {
            return json!({
                "content": [{"type": "text", "text": error}],
                "isError": true,
            });
        }
    };
    let metadata = match std::fs::metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) => {
            return json!({
                "content": [{"type": "text", "text": format!("failed to access {path:?}: {error}")}],
                "isError": true,
            });
        }
    };
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        return json!({
            "content": [{"type": "text", "text": format!("{path:?} exceeds the {MAX_SKILL_FILE_BYTES}-byte skill file limit")}],
            "isError": true,
        });
    }
    match std::fs::read_to_string(&file) {
        Ok(content) => json!({"content": [{"type": "text", "text": content}]}),
        Err(error) => json!({
            "content": [{"type": "text", "text": format!("failed to read {path:?}: {error}")}],
            "isError": true,
        }),
    }
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "list_skills",
                "description": "List every skill currently available to this session (global skills, plus this project's own skills if a project is open). Each entry has name/description/scope/startedFrom.",
                "inputSchema": {"type": "object", "properties": {}},
            },
            {
                "name": "read_skill",
                "description": "Read a skill's full SKILL.md content by its exact name, as returned by list_skills.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"],
                },
            },
            {
                "name": "list_skill_files",
                "description": "List supporting files currently present below a skill, such as references/ and scripts/. SKILL.md itself is read with read_skill.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"],
                },
            },
            {
                "name": "read_skill_file",
                "description": "Read a UTF-8 supporting file below a skill, such as references/guide.md or scripts/check.sh. The path must be relative to that skill.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "path": {"type": "string"},
                    },
                    "required": ["name", "path"],
                },
            },
        ]
    })
}

fn handle_request(args: &Args, method: &str, params: Option<&Value>) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "snapflowd-mcp", "version": "0.1.0"},
        })),
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => {
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .ok_or_else(|| (-32602, "missing required 'name' param".to_string()))?;
            match name {
                "list_skills" => Ok(list_skills_result(args)),
                "read_skill" => {
                    let skill_name = params
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("name"))
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| (-32602, "missing required 'name' argument".to_string()))?;
                    Ok(read_skill_result(args, skill_name))
                }
                "list_skill_files" => {
                    let skill_name = params
                        .and_then(|p| p.get("arguments"))
                        .and_then(|a| a.get("name"))
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| (-32602, "missing required 'name' argument".to_string()))?;
                    Ok(list_skill_files_result(args, skill_name))
                }
                "read_skill_file" => {
                    let arguments = params
                        .and_then(|p| p.get("arguments"))
                        .ok_or_else(|| (-32602, "missing required arguments".to_string()))?;
                    let skill_name = arguments
                        .get("name")
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| (-32602, "missing required 'name' argument".to_string()))?;
                    let path = arguments
                        .get("path")
                        .and_then(|p| p.as_str())
                        .ok_or_else(|| (-32602, "missing required 'path' argument".to_string()))?;
                    Ok(read_skill_file_result(args, skill_name, path))
                }
                other => Err((-32601, format!("unknown tool: {other}"))),
            }
        }
        other => Err((-32601, format!("Method not found: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("file has parent")).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn text_content(result: Value) -> String {
        result["content"][0]["text"]
            .as_str()
            .expect("text MCP response")
            .to_string()
    }

    #[test]
    fn supporting_files_are_live_discoverable_and_readable() {
        let global_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let skill_dir = project_dir.path().join(".skills").join("release");
        write_skill_file(
            &skill_dir.join("SKILL.md"),
            "---\nname: release\ndescription: release process\n---\n",
        );
        let args = Args {
            global_dir: global_dir.path().to_path_buf(),
            project_dir: Some(project_dir.path().to_path_buf()),
        };

        write_skill_file(
            &skill_dir.join("references").join("checklist.md"),
            "# Release checklist\n",
        );
        write_skill_file(
            &skill_dir.join("scripts").join("verify.sh"),
            "#!/bin/sh\necho verified\n",
        );

        let files: Vec<String> =
            serde_json::from_str(&text_content(list_skill_files_result(&args, "release"))).unwrap();
        assert_eq!(files, vec!["references/checklist.md", "scripts/verify.sh"]);
        assert_eq!(
            text_content(read_skill_file_result(
                &args,
                "release",
                "references/checklist.md"
            )),
            "# Release checklist\n"
        );
    }

    #[test]
    fn supporting_file_reader_rejects_path_traversal() {
        let skill_dir = tempfile::tempdir().unwrap();
        let error = resolve_skill_file(skill_dir.path(), "../outside.txt").unwrap_err();
        assert!(error.contains("traversal"));
    }
}

fn main() {
    let args = parse_args();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // `notifications/initialized` (and any other JSON-RPC
        // notification -- no `id` field) gets no response, per spec.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params");
        let response = match handle_request(&args, method, params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
