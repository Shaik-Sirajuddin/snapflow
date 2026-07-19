//! `skills-mcp-server`: a minimal, real MCP (Model Context Protocol)
//! server, spoken to over stdio, exposing panel-rust's discovered skills
//! (`skills_state::scan_skills_dir`) as two tools an ACP-side agent can
//! call mid-session: `list_skills` and `read_skill`.
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

use panel_rust::skills_state::{project_skills_dir, scan_skills_dir, SkillScope};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

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

fn list_skills_result(args: &Args) -> Value {
    let mut entries = scan_skills_dir(&args.global_dir, SkillScope::Global);
    if let Some(project_dir) = &args.project_dir {
        entries.extend(scan_skills_dir(
            &project_skills_dir(project_dir),
            SkillScope::Project,
        ));
    }
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
    let mut entries = scan_skills_dir(&args.global_dir, SkillScope::Global);
    if let Some(project_dir) = &args.project_dir {
        entries.extend(scan_skills_dir(
            &project_skills_dir(project_dir),
            SkillScope::Project,
        ));
    }
    let Some(entry) = entries.iter().find(|e| e.name == name) else {
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
        ]
    })
}

fn handle_request(args: &Args, method: &str, params: Option<&Value>) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "skills-mcp-server", "version": "0.1.0"},
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
                other => Err((-32601, format!("unknown tool: {other}"))),
            }
        }
        other => Err((-32601, format!("Method not found: {other}"))),
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
