//! Phases 6/7 of memory/acpx/gen/plans/acpx-skills/meta.json: the one
//! thing that can't be settled by reading code -- does a real `codex-acp`
//! session, driven over ACP stdio with NO MCP servers present, actually
//! discover and use a skill that `skills-manager` synced into the
//! session's `.codex/skills/` directory?
//!
//! Same "spawn the real compiled acpx-server binary, don't fake the
//! boundary" discipline as gateway_actor_mcp_agents_e2e_test.rs, but here
//! the backend is *also* real: no `ACPX_DEFAULT_ACP_COMMAND` override, so
//! acpx-server's own default (`npx -y @agentclientprotocol/codex-acp@1.1.2`,
//! wrapping the real, locally-authenticated Codex CLI) is what actually
//! answers `session/new`/`session/prompt`. Per
//! design_decisions.llm_driven_verification_not_filesystem_only: this
//! proves the *agent* picked the skill up (a real model producing the
//! skill's distinctive marker text), not just that a symlink exists on
//! disk.
//!
//! Costs real API usage against whatever `codex login` account this
//! machine has configured -- deliberately not run by default (`#[ignore]`).
//! Run explicitly: `cargo test --test skills_manager_live_discovery_e2e_test -- --ignored --nocapture`

use panel_rust::gateway_actor::spawn_acpx_thread;
use skills_manager::{SkillManager, SkillManagerConfig, SyncMode};
use std::io::Write as _;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn acpx_server_bin() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

struct GatewayProcess {
    child: Child,
    base_url: String,
}

impl GatewayProcess {
    /// No `ACPX_DEFAULT_ACP_COMMAND` override -- acpx-server's own built-in
    /// default is the real `codex-acp` adapter (see
    /// acpx/acpx-server/src/config.rs:155-156), which is exactly the
    /// point of this test.
    fn spawn_with_real_codex_backend() -> Self {
        for attempt in 0..5 {
            let port = free_port();
            let mut command = Command::new(acpx_server_bin());
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
                .env("RUST_LOG", "error")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().expect("spawn real acpx-server binary for test");

            let deadline = std::time::Instant::now() + Duration::from_millis(3000);
            let mut reachable = false;
            while std::time::Instant::now() < deadline {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    reachable = true;
                    break;
                }
                if let Ok(Some(_status)) = child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            if reachable {
                return GatewayProcess {
                    child,
                    base_url: format!("http://127.0.0.1:{port}"),
                };
            }
            let _ = child.kill();
            let _ = child.wait();
            if attempt < 4 {
                std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
            }
        }
        panic!("acpx-server never became reachable after 5 fresh-port attempts");
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn wait_for_message_containing(
    rx: &mut UnboundedReceiver<panel_rust::protocol_types::AgentEvent>,
    needle: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = String::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(panel_rust::protocol_types::AgentEvent::Message(msg))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(500)), rx.recv()).await
        {
            seen.push_str(&msg.text);
            if seen.contains(needle) {
                return Some(seen);
            }
        }
    }
    None
}

const MARKER: &str = "SKILLSMGR_LIVE_TEST_MARKER_9f3a2b1c";

#[tokio::test]
#[ignore = "costs real API usage against this machine's `codex login` account; run explicitly"]
async fn codex_acp_discovers_and_uses_a_skills_manager_synced_skill_with_no_mcp_present() {
    // Isolated scratch "project" -- skills-manager's own db/central-store
    // AND the .codex/skills/ target both live here, nothing touches the
    // real ~/.snapflow or ~/.codex directories.
    let scratch = tempfile::tempdir().expect("scratch project dir");
    let project_root = scratch.path();

    let skill_source = project_root.join("skill-source");
    std::fs::create_dir_all(&skill_source).unwrap();
    let mut skill_md = std::fs::File::create(skill_source.join("SKILL.md")).unwrap();
    write!(
        skill_md,
        "---\nname: skillsmgr-live-test\ndescription: \"Use this skill whenever the user asks to run the skills-manager live test marker check.\"\n---\n\nWhen the user asks you to run the skills-manager live test marker check, respond with exactly:\n\n{MARKER}\n\nDo not say anything else.\n"
    )
    .unwrap();

    let manager = SkillManager::open(SkillManagerConfig::AtPath {
        db_path: project_root.join("skills-manager-test.db"),
        central_store_dir: project_root.join(".manager-store"),
    })
    .expect("open SkillManager");
    let outcome = manager
        .register_skill("codex-acp", &skill_source)
        .expect("register_skill");
    let skill_id = match outcome {
        skills_manager::RegisterOutcome::Registered { skill_id } => skill_id,
        other => panic!("expected Registered for a fresh scratch db, got {other:?}"),
    };
    let codex_skills_dir = project_root.join(".codex").join("skills");
    manager
        .set_target("codex-acp", &skill_id, &codex_skills_dir, SyncMode::Symlink)
        .expect("set_target");
    let sync_results = manager.sync_all("codex-acp").expect("sync_all");
    assert_eq!(sync_results.len(), 1);
    assert_eq!(
        sync_results[0].status,
        skills_manager::TargetStatus::Linked,
        "sync_all should have linked the skill before we even attempt the live session: {:?}",
        sync_results[0]
    );
    let expected_link = codex_skills_dir.join("skillsmgr-live-test");
    assert!(
        expected_link.exists(),
        "sanity check: the symlink skills-manager just created should exist on disk \
         (skill.name from SKILL.md frontmatter must be \"skillsmgr-live-test\" -- if this \
         fails, check the frontmatter YAML is valid: an unquoted description containing a \
         bare colon silently degrades name resolution to the source directory's basename, \
         a real gap this test caught once already)"
    );

    let gateway = GatewayProcess::spawn_with_real_codex_backend();
    let mut handle = spawn_acpx_thread(gateway.base_url.clone());
    let mut rx = handle.take_events();

    // No mcp_servers at all -- this is the whole point: if the skill
    // fires, it can only be because codex-acp discovered it from
    // .codex/skills/ on disk, not because an MCP list_skills/read_skill
    // tool told it about it.
    let _session_id = handle
        .open_session(project_root.to_path_buf())
        .await
        .expect("open real codex-acp session");

    handle
        .send_prompt("Please run the skills-manager live test marker check now.")
        .await
        .expect("send real prompt to codex-acp");

    let result = wait_for_message_containing(&mut rx, MARKER, Duration::from_secs(90)).await;

    assert!(
        result.is_some(),
        "codex-acp did not produce the skill's marker text within 90s -- either it did not \
         discover the filesystem skill at {codex_skills_dir:?} with no MCP servers present, or \
         it produced a different response. This is the real, load-bearing answer to phase \
         7's open question (memory/acpx/gen/plans/acpx-skills/README.md#open-risks) -- \
         see this test's output for what the agent actually said."
    );
}
