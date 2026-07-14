//! `acpx-selftest`: a standalone, publishable diagnostic CLI for
//! operators/CI to run against an **already-deployed** `acpx-server`
//! instance over the network.
//!
//! This is deliberately *not* `cargo test`. The in-process integration
//! suites (e.g. `acpx-server/tests/http_ws_transport_test.rs`,
//! `acpx-server/tests/binary_self_test.rs`) spawn their own short-lived
//! server/backend inside the test run and only ever exist at build/CI
//! time, against source you have checked out. `acpx-selftest` is the
//! opposite: it is a small, standalone binary you install/ship alongside
//! `acpx-server` and run *after* deployment, black-box, purely over HTTP
//! against whatever gateway is already listening -- local, staging, or a
//! remote production host -- with no access to (or dependency on) that
//! instance's source, process, or logs. Point it at a URL, get a
//! PASS/FAIL smoke check and a process exit code, wire it into a
//! deploy/health-check pipeline (`if ! acpx-selftest; then ...`).
//!
//! Checks performed (see each `check_*` fn for the exact JSON-RPC
//! envelope, matching `acpx-core/src/router.rs`'s `dispatch_native` /
//! `dispatch_session_new` / `dispatch_proxied` shapes exactly):
//! 1. `session/list` via `POST <target>/rpc` -- mandatory, proves the
//!    gateway-native API surface is up and answering JSON-RPC at all.
//! 2. `agents/list` via `POST <target>/rpc` -- mandatory, proves the
//!    agent registry is reachable from inside the running gateway.
//! 3. (only when `ACPX_SELFTEST_FULL=1`) a full `session/new` ->
//!    `session/prompt` -> `session/close` round trip -- best-effort:
//!    a target with no real ACP adapter/API key configured is expected
//!    to answer with a backend-specific JSON-RPC `error` (that still
//!    counts as "the gateway is alive and proxying correctly"), so only
//!    transport-level failures (connection refused, non-JSON body,
//!    malformed JSON-RPC envelope missing both `result` and `error`) are
//!    treated as a hard failure here.

use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

/// Default target when neither `--target` nor `ACPX_SELFTEST_TARGET` is
/// given -- matches `acpx-server`'s own default HTTP bind address (see
/// `config.rs`'s `ServerConfig::http_bind_addr` default) so a bare
/// `acpx-selftest` run against a locally-started default-config server
/// just works.
const DEFAULT_TARGET: &str = "http://127.0.0.1:8790";

/// Result of a single named check, tracked separately from "did the HTTP
/// call itself error out" so the summary can report a readable per-check
/// PASS/FAIL line regardless of which layer failed.
struct CheckResult {
    name: &'static str,
    passed: bool,
    detail: String,
}

impl CheckResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
        }
    }

    fn print(&self) {
        let label = if self.passed { "PASS" } else { "FAIL" };
        println!("[{label}] {}: {}", self.name, self.detail);
    }
}

/// Resolves the target base URL: `--target <url>` wins over
/// `ACPX_SELFTEST_TARGET`, which wins over `DEFAULT_TARGET`. Trailing
/// slashes are trimmed so `format!("{target}/rpc")` never double-slashes.
fn resolve_target() -> String {
    let mut args = std::env::args().skip(1);
    let mut cli_target = None;
    while let Some(arg) = args.next() {
        if arg == "--target" {
            cli_target = args.next();
        } else if let Some(value) = arg.strip_prefix("--target=") {
            cli_target = Some(value.to_string());
        }
    }
    let target = cli_target
        .or_else(|| std::env::var("ACPX_SELFTEST_TARGET").ok())
        .unwrap_or_else(|| DEFAULT_TARGET.to_string());
    target.trim_end_matches('/').to_string()
}

/// POSTs one JSON-RPC request to `<target>/rpc` and parses the response
/// body as JSON. Transport-level failures (connection refused, timeout,
/// non-JSON body) surface as `Err` with a human-readable message; a
/// well-formed JSON-RPC response -- success or error -- always returns
/// `Ok`, since a JSON-RPC `error` is a valid envelope, not a transport
/// failure (see `acpx-server/src/transport/http.rs`'s `rpc_handler`,
/// which always answers `200 OK` and reports failures via the body's
/// `error` field).
async fn post_rpc(
    client: &reqwest::Client,
    target: &str,
    request: &Value,
) -> Result<Value, String> {
    let response = client
        .post(format!("{target}/rpc"))
        .json(request)
        .send()
        .await
        .map_err(|err| format!("transport error: {err}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|err| format!("malformed JSON-RPC envelope (HTTP status {status}): {err}"))?;
    if body.get("jsonrpc").is_none()
        || (body.get("result").is_none() && body.get("error").is_none())
    {
        return Err(format!("malformed JSON-RPC envelope: {body}"));
    }
    Ok(body)
}

/// Mandatory check 1: `session/list`, a gateway-native method that
/// answers purely from in-process state (no backend agent involved), so
/// a `result` here proves the gateway's JSON-RPC dispatch is alive.
async fn check_session_list(client: &reqwest::Client, target: &str) -> CheckResult {
    let request = json!({
        "jsonrpc": "2.0",
        "id": "selftest-session-list",
        "method": "session/list",
        "params": {}
    });
    match post_rpc(client, target, &request).await {
        Ok(body) if body.get("result").is_some() => {
            let count = body["result"]["sessions"]
                .as_array()
                .map(|s| s.len())
                .unwrap_or(0);
            CheckResult::pass("session/list", format!("{count} session(s) reported"))
        }
        Ok(body) => CheckResult::fail(
            "session/list",
            format!("gateway returned a JSON-RPC error: {}", body["error"]),
        ),
        Err(err) => CheckResult::fail("session/list", err),
    }
}

/// Mandatory check 2: `agents/list`, a gateway-native method that reads
/// through the on-disk agent registry, so a `result` here proves that
/// path (distinct from the session registry `session/list` exercises) is
/// reachable too.
async fn check_agents_list(client: &reqwest::Client, target: &str) -> CheckResult {
    let request = json!({
        "jsonrpc": "2.0",
        "id": "selftest-agents-list",
        "method": "agents/list",
        "params": {}
    });
    match post_rpc(client, target, &request).await {
        Ok(body) if body.get("result").is_some() => {
            let count = body["result"]["agents"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            CheckResult::pass(
                "agents/list",
                format!("{count} registered agent(s) reported"),
            )
        }
        Ok(body) => CheckResult::fail(
            "agents/list",
            format!("gateway returned a JSON-RPC error: {}", body["error"]),
        ),
        Err(err) => CheckResult::fail("agents/list", err),
    }
}

/// Optional (`ACPX_SELFTEST_FULL=1`) check: a full `session/new` ->
/// `session/prompt` -> `session/close` round trip through the *proxied*
/// path, i.e. all the way out to a real spawned backend agent process --
/// something the two mandatory gateway-native checks above never
/// exercise. Since there's no guarantee the target has a real ACP
/// adapter + API key configured, a backend-specific JSON-RPC `error` at
/// any step is tolerated and reported as a pass (it still proves the
/// gateway spawned/proxied to a backend and got a well-formed response
/// back); only a transport-level failure is a hard fail.
async fn check_full_round_trip(client: &reqwest::Client, target: &str) -> CheckResult {
    let new_request = json!({
        "jsonrpc": "2.0",
        "id": "selftest-session-new",
        "method": "session/new",
        "params": {"cwd": std::env::temp_dir().to_string_lossy()}
    });
    let new_response = match post_rpc(client, target, &new_request).await {
        Ok(body) => body,
        Err(err) => return CheckResult::fail("session/new+prompt+close", err),
    };
    let Some(session_id) = new_response["result"]["sessionId"].as_str() else {
        // A backend-specific error (e.g. no adapter/API key configured)
        // is an expected, tolerated outcome for this best-effort check --
        // the gateway itself round-tripped a well-formed JSON-RPC error,
        // which is exactly what a proxied call to a misconfigured
        // backend should do.
        return CheckResult::pass(
            "session/new+prompt+close",
            format!(
                "session/new returned no sessionId, tolerated as a backend-specific outcome: {}",
                new_response.get("error").unwrap_or(&new_response)
            ),
        );
    };

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": "selftest-session-prompt",
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "acpx-selftest liveness ping"}]
        }
    });
    let prompt_outcome = match post_rpc(client, target, &prompt_request).await {
        Ok(body) => format!("session/prompt responded: {}", summarize(&body)),
        Err(err) => return CheckResult::fail("session/new+prompt+close", err),
    };

    let close_request = json!({
        "jsonrpc": "2.0",
        "id": "selftest-session-close",
        "method": "session/close",
        "params": {"sessionId": session_id}
    });
    let close_outcome = match post_rpc(client, target, &close_request).await {
        Ok(body) => format!("session/close responded: {}", summarize(&body)),
        Err(err) => return CheckResult::fail("session/new+prompt+close", err),
    };

    CheckResult::pass(
        "session/new+prompt+close",
        format!("sessionId={session_id}; {prompt_outcome}; {close_outcome}"),
    )
}

/// Short "result" or "error: <message>" summary of a JSON-RPC response
/// body, for compact detail strings.
fn summarize(body: &Value) -> String {
    if let Some(error) = body.get("error") {
        format!("error ({})", error.get("message").unwrap_or(error))
    } else {
        "ok".to_string()
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let target = resolve_target();
    let full = std::env::var("ACPX_SELFTEST_FULL").as_deref() == Ok("1");

    println!("acpx-selftest: target={target} full={full}");

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            println!("[FAIL] client-init: could not build HTTP client: {err}");
            println!("OVERALL: FAIL (0/1 mandatory checks passed)");
            return ExitCode::FAILURE;
        }
    };

    let mut results = vec![
        check_session_list(&client, &target).await,
        check_agents_list(&client, &target).await,
    ];
    if full {
        results.push(check_full_round_trip(&client, &target).await);
    }

    for result in &results {
        result.print();
    }

    // Only the first two (session/list, agents/list) are mandatory per
    // the tool's contract; the optional full round trip is reported but
    // never gates the exit code on its own.
    let mandatory_passed = results.iter().take(2).filter(|r| r.passed).count();
    let all_mandatory_pass = mandatory_passed == 2;
    let total = results.len();
    let passed = results.iter().filter(|r| r.passed).count();

    if all_mandatory_pass {
        println!("OVERALL: PASS ({passed}/{total} checks passed)");
        ExitCode::SUCCESS
    } else {
        println!("OVERALL: FAIL ({passed}/{total} checks passed, {mandatory_passed}/2 mandatory)");
        ExitCode::FAILURE
    }
}
