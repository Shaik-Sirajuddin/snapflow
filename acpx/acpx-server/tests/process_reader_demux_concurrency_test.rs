//! **True end-to-end proof for `process_reader_demux`** (phase 1 of
//! `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`), spawning
//! the real, already-compiled `acpx-server` binary and driving it purely
//! from outside the process over real HTTP -- same "spawn the real binary"
//! pattern as `acp_bridge_binary_test.rs`.
//!
//! The bottleneck this closes (`memory/acpx/tasks/zed_integration.yaml`
//! task 7): with `tenant_process_isolation=false` and
//! `session_process_isolation=false` (the live default), every session for
//! one agent shares one physical backend process, and the pre-fix code
//! held that process's own lock across the *entire* write + blocking-read
//! turn -- so two sessions sharing a process fully serialized behind each
//! other's whole turn, not just the request write. `ACPX_PROCESS_READER_
//! DEMUX=1` fixes this by registering a response id against a per-process
//! reader task instead of holding the lock across the read.
//!
//! Uses a real, slow-but-well-behaved stand-in agent (a tiny Python
//! script, `PROMPT_DELAY` seconds per `session/prompt`) so two concurrent
//! turns on one shared process have a real, measurable window to either
//! overlap or serialize in.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::{Child, Command};

/// Real turn latency simulated by the stand-in backend for every
/// `session/prompt` -- long enough that two *serialized* turns (~2x this)
/// and two *overlapping* turns (~1x this) are unambiguously distinguishable
/// over real wall-clock HTTP round trips, short enough to keep this test
/// fast.
const PROMPT_DELAY_SECS: f64 = 1.2;

/// **Must itself be concurrent, not a naive blocking read-process-reply
/// loop.** A real ACP adapter like codex-acp already queues by request id
/// and can have several requests genuinely in flight on one process at
/// once (see `memory/acpx/tasks/zed_integration.yaml` task 7's own note:
/// "the backend side already supports concurrent multi-session traffic on
/// one process ... acpx-conductor's client side does not take advantage
/// of it"). A stand-in that reads one line, blocks in `time.sleep`, *then*
/// reads the next line could never demonstrate overlap no matter how
/// acpx's own side behaves -- it would still show ~2x serialized latency
/// for two concurrent prompts even with a perfect fix on the acpx side,
/// which would make this whole test meaningless. `asyncio` here answers
/// every `session/prompt` on its own independent sleeping task while
/// continuing to read new lines off stdin the whole time, exactly the
/// concurrency real adapters already have.
const STAND_IN_AGENT_SCRIPT: &str = r#"
import asyncio, sys, json, uuid

delay = float(sys.argv[1]) if len(sys.argv) > 1 else 1.0
write_lock = asyncio.Lock()

async def send(obj):
    line = json.dumps(obj) + "\n"
    async with write_lock:
        sys.stdout.write(line)
        sys.stdout.flush()

async def handle(req):
    rid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        await send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [],
        }})
    elif method == "session/new":
        await send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": str(uuid.uuid4())}})
    elif method == "session/prompt":
        await asyncio.sleep(delay)
        await send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
    elif method == "session/close":
        await send({"jsonrpc": "2.0", "id": rid, "result": {}})
    else:
        await send({"jsonrpc": "2.0", "id": rid, "result": {}})

async def main():
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    loop = asyncio.get_event_loop()
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)
    while True:
        line = await reader.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        asyncio.create_task(handle(req))

asyncio.run(main())
"#;

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    )
}

fn write_temp_file(prefix: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
    let mut file = std::fs::File::create(&path).expect("create temp file");
    file.write_all(contents.as_bytes())
        .expect("write temp file");
    path
}

async fn ephemeral_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    addr
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real binary never opened its HTTP listener");
}

struct ServerGuard {
    child: Child,
    _script_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Spawns the real `acpx-server` binary against the stand-in agent script,
/// with `ACPX_PROCESS_READER_DEMUX` set per `demux_enabled`. Every session
/// created against the default agent shares one physical backend process
/// (no isolation flags set), which is exactly the scenario this whole
/// phase targets.
async fn spawn_server(demux_enabled: bool) -> (ServerGuard, SocketAddr, reqwest::Client) {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-demux-stand-in-agent", STAND_IN_AGENT_SCRIPT);
    let db_path = write_temp_file("acpx-demux-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!(
            "python3 {} {PROMPT_DELAY_SECS}",
            script_path.display()
        ),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    .env(
        "ACPX_PROCESS_READER_DEMUX",
        if demux_enabled { "1" } else { "0" },
    )
    .env_remove("ACPX_AUTH_TOKEN")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let guard = ServerGuard {
        child,
        _script_path: script_path,
    };
    wait_for_listener(addr).await;
    (guard, addr, reqwest::Client::new())
}

async fn rpc(client: &reqwest::Client, addr: SocketAddr, body: Value) -> Value {
    client
        .post(format!("http://{addr}/rpc"))
        .json(&body)
        .send()
        .await
        .expect("POST /rpc")
        .json::<Value>()
        .await
        .expect("json body")
}

async fn session_new(client: &reqwest::Client, addr: SocketAddr, id: i64) -> String {
    let response = rpc(
        client,
        addr,
        json!({
            "jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": "/tmp", "mcpServers": []}
        }),
    )
    .await;
    response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {response:?}"))
        .to_string()
}

async fn session_prompt(client: &reqwest::Client, addr: SocketAddr, id: i64, session_id: &str) -> Value {
    rpc(
        client,
        addr,
        json!({
            "jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": []}
        }),
    )
    .await
}

/// **The core overlap proof.** Two sessions on the same shared backend
/// process each get a `session/prompt` turn in flight at (almost) the same
/// time. With `process_reader_demux` on, both turns run concurrently on
/// the shared process, so the pair finishes in close to *one* turn's
/// duration, not two serialized turns.
#[tokio::test]
async fn demux_on_two_concurrent_sessions_on_one_shared_process_overlap_in_wall_time() {
    let (guard, addr, client) = spawn_server(true).await;

    let sid_a = session_new(&client, addr, 1).await;
    let sid_b = session_new(&client, addr, 2).await;

    let started = Instant::now();
    let (client_a, addr_a, sid_a_c) = (client.clone(), addr, sid_a.clone());
    let task_a = tokio::spawn(async move { session_prompt(&client_a, addr_a, 3, &sid_a_c).await });
    let (client_b, addr_b, sid_b_c) = (client.clone(), addr, sid_b.clone());
    let task_b = tokio::spawn(async move { session_prompt(&client_b, addr_b, 4, &sid_b_c).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let elapsed = started.elapsed();
    let result_a = result_a.expect("task a must not panic");
    let result_b = result_b.expect("task b must not panic");

    assert_eq!(result_a["result"]["stopReason"], json!("end_turn"), "{result_a:?}");
    assert_eq!(result_b["result"]["stopReason"], json!("end_turn"), "{result_b:?}");

    let one_turn = Duration::from_secs_f64(PROMPT_DELAY_SECS);
    let two_turns_serialized = one_turn * 2;
    assert!(
        elapsed < one_turn + Duration::from_millis(700),
        "two concurrent session/prompt calls on one shared backend process took {elapsed:?} \
         with process_reader_demux ON -- expected close to one turn's duration ({one_turn:?}), \
         proving they overlapped in wall time rather than serializing behind each other's whole \
         turn (a regression back to serialization would push this toward {two_turns_serialized:?})"
    );

    drop(guard);
}

/// **The baseline this fixes.** Same scenario, `process_reader_demux` off
/// (the current production default) -- the two turns must fully serialize,
/// taking close to *two* turns' duration. This pins the pre-fix behavior
/// so a regression that silently makes the flag a no-op is caught here,
/// not just by the "on" test passing for unrelated reasons.
#[tokio::test]
async fn demux_off_two_concurrent_sessions_on_one_shared_process_serialize() {
    let (guard, addr, client) = spawn_server(false).await;

    let sid_a = session_new(&client, addr, 1).await;
    let sid_b = session_new(&client, addr, 2).await;

    let started = Instant::now();
    let (client_a, addr_a, sid_a_c) = (client.clone(), addr, sid_a.clone());
    let task_a = tokio::spawn(async move { session_prompt(&client_a, addr_a, 3, &sid_a_c).await });
    let (client_b, addr_b, sid_b_c) = (client.clone(), addr, sid_b.clone());
    let task_b = tokio::spawn(async move { session_prompt(&client_b, addr_b, 4, &sid_b_c).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let elapsed = started.elapsed();
    result_a.expect("task a must not panic");
    result_b.expect("task b must not panic");

    let one_turn = Duration::from_secs_f64(PROMPT_DELAY_SECS);
    assert!(
        elapsed >= one_turn + Duration::from_millis(700),
        "two concurrent session/prompt calls on one shared backend process took only {elapsed:?} \
         with process_reader_demux OFF -- expected close to two serialized turns' duration \
         ({:?}); if this genuinely overlapped without the fix enabled, the baseline this test \
         pins no longer holds and the 'on' test's proof is meaningless",
        one_turn * 2
    );

    drop(guard);
}

/// **Sub-1s launch/resume under concurrent load.** While two other
/// sessions on the same shared backend process each have a slow
/// (`PROMPT_DELAY_SECS`) `session/prompt` turn genuinely in flight, a
/// brand new session's `session/new` (a launch) and a third session's own
/// `session/prompt` turn (a resume of interaction on that session) must
/// each still resolve quickly on their own terms -- `session/new` in well
/// under a second (it never touches the artificial prompt delay at all),
/// and the third `session/prompt` close to its own turn's duration, not
/// stacked behind the other two.
#[tokio::test]
async fn demux_on_launch_and_resume_stay_responsive_under_concurrent_load() {
    let (guard, addr, client) = spawn_server(true).await;

    let sid_a = session_new(&client, addr, 1).await;
    let sid_b = session_new(&client, addr, 2).await;

    // Get A and B's turns genuinely in flight before measuring anything
    // else against them.
    let (client_a, addr_a, sid_a_c) = (client.clone(), addr, sid_a.clone());
    let task_a = tokio::spawn(async move { session_prompt(&client_a, addr_a, 3, &sid_a_c).await });
    let (client_b, addr_b, sid_b_c) = (client.clone(), addr, sid_b.clone());
    let task_b = tokio::spawn(async move { session_prompt(&client_b, addr_b, 4, &sid_b_c).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!task_a.is_finished() && !task_b.is_finished(), "A and B's turns should still be in flight");

    // Launch: a brand new session/new on the same shared agent, while A
    // and B's turns are genuinely mid-flight on the process it shares.
    let launch_started = Instant::now();
    let sid_c = session_new(&client, addr, 5).await;
    let launch_elapsed = launch_started.elapsed();
    assert!(
        launch_elapsed < Duration::from_secs(1),
        "session/new (launch) took {launch_elapsed:?} while two other sessions' turns were \
         in flight on the same shared backend process -- expected sub-1s; a regression back to \
         holding the per-process lock across a full turn would block this behind A/B's turns"
    );

    // Resume: C's own first prompt turn, still while A and B may still be
    // finishing theirs -- must resolve close to its own turn's duration,
    // not stacked behind A and B's.
    let resume_started = Instant::now();
    let result_c = session_prompt(&client, addr, 6, &sid_c).await;
    let resume_elapsed = resume_started.elapsed();
    assert_eq!(result_c["result"]["stopReason"], json!("end_turn"), "{result_c:?}");
    assert!(
        resume_elapsed < Duration::from_secs_f64(PROMPT_DELAY_SECS) + Duration::from_millis(700),
        "session/prompt on a third session took {resume_elapsed:?} while sharing the backend \
         process with two other in-flight turns -- expected close to one turn's own duration \
         ({PROMPT_DELAY_SECS}s), not stacked behind the others"
    );

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    assert_eq!(
        result_a.expect("task a must not panic")["result"]["stopReason"],
        json!("end_turn")
    );
    assert_eq!(
        result_b.expect("task b must not panic")["result"]["stopReason"],
        json!("end_turn")
    );

    drop(guard);
}
