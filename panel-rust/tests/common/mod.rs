//! Shared fixtures for the real-process e2e harnesses.
//!
//! Every test file here follows this crate's "spawn the real binary,
//! don't fake the boundary" discipline, which means they all need the
//! same four things: locate the built binaries, get a port, get an
//! `acpx-server` running on it, and register a backend the gateway can
//! actually route to. Those four had been copied into each file
//! independently -- `free_port` into five, `acpx_server_bin` into five,
//! `spawn_acpx_server_with_retry` into three, and the admin-plane
//! registration into four under two different names.
//!
//! That duplication was not merely untidy, it had already cost us a real
//! bug. `free_port`'s TOCTOU gap (documented on the function below) was
//! diagnosed once and patched with a *retry wrapper*, and then both the
//! broken helper and its workaround were copied onward together -- so the
//! underlying allocation was never fixed, because fixing it meant editing
//! five files while patching locally meant editing one. Consolidating here
//! is what makes a real fix a one-place change.
//!
//! Rust compiles this module separately into each test binary, so any
//! given binary uses only part of it; `dead_code` is allowed for that
//! reason and not because anything here is unused overall.
#![allow(dead_code)]

use acpx_client::ext::admin::AdminClient;
use acpx_proto::admin::CustomAgentSpec;
use panel_rust::gateway_actor::spawn_acpx_thread;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Resolves the real, already-built `acpx-server` binary next to this
/// crate's own checkout -- mirrors `panel-rust/src/agent_bridge.rs`'s
/// `resolve_agent_command`'s dev-checkout-relative-path pattern.
pub fn acpx_server_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

/// The compiled `rui-mock-agent` this crate builds for its own harnesses.
pub fn mock_agent_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/rui-mock-agent")
}

/// Binds an ephemeral TCP port synchronously (std, not tokio -- this runs
/// before any runtime is guaranteed up), reads the number the OS assigned,
/// then drops the listener so `acpx-server` can bind that port itself.
///
/// **This is racy by construction and cannot be made safe in isolation.**
/// Between the drop here and the child's own bind there is a window in
/// which any other process -- including a concurrently running test doing
/// exactly this -- can take the port. That is the confirmed cause of the
/// connection-refused flakes seen across these harnesses under parallel
/// load: an identical tree yields a different number of failures, in
/// different tests, on each run, and is clean at `--test-threads=1`.
///
/// [`spawn_acpx_server_with_retry`] narrows the window but cannot close
/// it. Closing it properly means never releasing the port between
/// allocation and use -- either by handing the listener itself to the
/// child, or by having `acpx-server` bind `:0` and report back the port it
/// got. That fix belongs here, in this one function, which is the point of
/// this module existing.
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// Spawns a real `acpx-server` child, retrying the whole
/// pick-port/spawn/wait-for-connect cycle (bounded at 5 attempts) when the
/// process never becomes reachable within one attempt's window -- which in
/// practice means it lost [`free_port`]'s race and died on bind.
///
/// This is mitigation, not a fix: it lowers the probability of a collision
/// surviving to a test failure, it does not remove the race. See
/// [`free_port`].
pub fn spawn_acpx_server_with_retry(configure: impl Fn(&mut Command, u16)) -> (Child, String) {
    for attempt in 0..5 {
        let port = free_port();
        let mut command = Command::new(acpx_server_bin());
        configure(&mut command, port);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .expect("spawn real acpx-server binary for test");

        let deadline = Instant::now() + Duration::from_millis(3000);
        let mut reachable = false;
        while Instant::now() < deadline {
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
            return (child, format!("http://127.0.0.1:{port}"));
        }
        let _ = child.kill();
        let _ = child.wait();
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
    }
    panic!("acpx-server never became reachable after 5 attempts");
}

/// Blocks until the admin plane accepts TCP connections, bounded at 3s.
/// The admin routes are served by the same process the caller just
/// spawned, so this is a startup wait rather than a health check.
fn wait_for_admin(admin_port: u16) {
    let deadline = Instant::now() + Duration::from_millis(3000);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", admin_port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Registers a backend as a durable admin-plane custom agent
/// (`POST /admin/agents/custom`, see `acpx-server/src/transport/admin.rs`).
///
/// PROF-4 (`profile-only-backend-selection`): this is the replacement for
/// setting `ACPX_BACKEND_CMD` on the gateway process, removed from
/// production in PROF-3. `Router::ensure_custom_agent_registered` ->
/// `Supervisor::register` puts the entry under whatever `agent_id` is
/// given, so a profile naming that id resolves to it unchanged -- which is
/// why harnesses that already had an `agent_id: "..."` profile needed no
/// assertion changes, only a different way of registering the backend.
///
/// A profile's `launch_overrides` cannot express this: it only injects env
/// vars into whatever command the registry already holds for an agent id,
/// so it can never redirect *which binary* runs. The admin plane is the
/// only route that can point an agent id at a test's own backend.
pub async fn register_custom_agent(
    admin_port: u16,
    admin_token: &str,
    agent_id: &str,
    command: &str,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) {
    wait_for_admin(admin_port);
    let admin = AdminClient::new(format!("http://127.0.0.1:{admin_port}"), admin_token);
    let result = admin
        .create_custom_agent(&CustomAgentSpec {
            id: agent_id.to_owned(),
            name: agent_id.to_owned(),
            command: command.to_owned(),
            args,
            env,
            cwd: None,
        })
        .await;
    // Idempotent: a caller that spawns a *second* `acpx-server` pointed at
    // the same durable `ACPX_DB_PATH` (e.g. a restart-persistence check --
    // see `gateway_actor_mcp_agents_e2e_test.rs`'s `mcp_server_oauth_flow_
    // completes_through_a_real_authorization_server`) re-registers the
    // exact same custom agent against a store that already durably
    // persisted it from the first spawn. That is a real, expected outcome
    // of "this survives a restart," not a caller error, so a 409 here
    // means the registration this call wanted already exists -- treat it
    // the same as success rather than panicking every restart-persistence
    // test that reuses this helper.
    if let Err(acpx_client::ext::admin::AdminClientError::Response { status: 409, .. }) = &result {
        return;
    }
    result.expect("admin/agents/custom create");
}

/// Registers a shell-script backend under `agent_id` -- the shape the
/// stand-in-backend harnesses use, where the "agent" is a script written
/// to a temp dir rather than a compiled binary.
pub async fn register_stand_in_backend(
    admin_port: u16,
    admin_token: &str,
    agent_id: &str,
    script_path: &Path,
) {
    register_custom_agent(
        admin_port,
        admin_token,
        agent_id,
        "sh",
        vec![script_path.to_string_lossy().into_owned()],
        BTreeMap::new(),
    )
    .await;
}

/// Registers `rui-mock-agent` as a custom agent AND creates a profile
/// naming it, returning the profile name.
///
/// This is the full "select the mock the way a real backend is selected"
/// path: callers open sessions with `open_session_with_profile(cwd,
/// &profile_name, ..)` rather than relying on a gateway-level env var, so
/// the mock travels through the same `_acpx.profile` resolution a real
/// backend does instead of a shortcut around it.
///
/// `extra_env` layers onto the custom agent's spawn env (e.g.
/// `RUI_MOCK_AGENT_EVENT_LOG`) on top of `RUI_MOCK_AGENT_PERSONA`, which
/// is always set from `persona`.
pub async fn provision_mock_profile(
    base_url: &str,
    admin_port: u16,
    admin_token: &str,
    persona: &str,
    extra_env: BTreeMap<String, String>,
) -> String {
    let custom_agent_id = format!("mock-{persona}");
    let mut env = extra_env;
    env.insert("RUI_MOCK_AGENT_PERSONA".to_owned(), persona.to_owned());
    register_custom_agent(
        admin_port,
        admin_token,
        &custom_agent_id,
        &mock_agent_bin().to_string_lossy(),
        Vec::new(),
        env,
    )
    .await;

    let handle = spawn_acpx_thread(base_url.to_owned());
    handle
        .create_profile(serde_json::json!({
            "name": persona,
            "agent_id": custom_agent_id,
        }))
        .await
        .expect("profiles/create for the mock profile");
    persona.to_owned()
}
