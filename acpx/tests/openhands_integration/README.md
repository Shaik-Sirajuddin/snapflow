# acpx <-> OpenHands integration tests

Real, full-lifecycle end-to-end coverage for the acpx/OpenHands
integration described in `../../scripts/openhands-acpx-claude.sh` and
`../../scripts/openhands-acpx-codex.sh`: OpenHands's own agent-server
spawning `acpx-server` as its ACP subprocess (via `ACPAgent(acp_server=
"custom", acp_command=[<wrapper script>])`), which in turn spawns a real
`claude-agent-acp`/`codex-acp` adapter -- not a mock, not just
`acpx-server` tested in isolation.

Two layers, independent of each other:

- `acp_stdio_client.py` -- a minimal, dependency-free (stdlib-only) async
  ACP-over-stdio client. Drives `acpx-server` (via either wrapper script,
  or the raw binary directly) exactly the way any ACP client would, with
  no OpenHands agent-server involved at all. Useful for isolating "is
  acpx-server/the wrapper script/the real adapter working" from "is the
  OpenHands integration specifically working".
- `openhands_sdk_driver.py` + `test_openhands_acpx_e2e.py` -- reuses
  OpenHands's own real `openhands-sdk` client library
  (`openhands.sdk.Conversation`/`RemoteWorkspace`/
  `openhands.sdk.agent.ACPAgent`) to drive a conversation through a real,
  already-running agent-server end to end. This is the suite this
  README is mainly about.

## Prerequisites

1. A release `acpx-server` binary: `cd .. && cargo build --release -p
   acpx-server` (the wrapper scripts default to
   `acpx/target/release/acpx-server`; override with `ACPX_SERVER_BIN`).
2. Real Claude/Codex credentials already logged in on this host
   (`claude login` / `codex login`, or the equivalent auth files under
   `~/.claude`/`~/.codex`) -- these tests hit real models and cost real
   money/quota. There is no mock backend option here on purpose: the
   entire point is proving the real chain works, see `../../README.md`'s
   "Status" section for where the black-box/synthetic-backend coverage
   already lives instead (`acpx-server/tests/binary_self_test.rs` et
   al.).
3. A running OpenHands agent-server + agent-canvas dev stack (the
   `agent-server`/`ingress.mjs`/`static-server.mjs` processes an operator
   starts via the normal OpenHands dev workflow). This suite attaches to
   one rather than starting its own -- see "why attach, not spawn" below.

## Running

From the `acpx/` directory, so the wrapper-script paths
`openhands_sdk_driver.py` resolves relative to `ACPX_ROOT` are correct:

```sh
uv run --with openhands-sdk==1.29.0 --with pytest \
    pytest tests/openhands_integration -v
```

`--with openhands-sdk==1.29.0` should match whatever version the running
agent-server itself was launched with (`uvx --from
openhands-agent-server==<version> ... agent-server`) -- these tests
import the real SDK client classes, so a version mismatch against the
live server is exactly the kind of drift they exist to catch, not
something to paper over with a looser pin.

If no agent-server/agent-canvas stack is running (or the session API key
can't be resolved), every test in this file `SKIP`s cleanly with a
message explaining why, rather than erroring during collection -- see
`conftest.py`'s fixtures.

### Pointing at a specific agent-server

```sh
uv run --with openhands-sdk==1.29.0 --with pytest \
    pytest tests/openhands_integration -v \
    --openhands-host http://127.0.0.1:18000 \
    --openhands-session-api-key <key>
```

Omit `--openhands-session-api-key` to fall back to the
`OPENHANDS_SESSION_API_KEY` env var, or (last resort, local-dev-only)
auto-discovery off the already-running `agent-canvas` static-file-server
process's own `--session-api-key` argument -- see
`openhands_sdk_driver.discover_session_api_key`'s doc comment.

## What each test actually proves

`test_acpx_backend_end_to_end_via_openhands_sdk` (parametrized over the
Claude and Codex backends):

1. Starts a real conversation against the real running agent-server,
   with a real `ACPAgent(acp_server="custom", acp_command=[wrapper
   script])`.
2. Confirms the server-persisted `agent` block (`GET /api/conversations/
   {id}`) actually reflects `acp_server="custom"` and the exact
   `acp_command` requested -- proves OpenHands didn't silently fall back
   to some pre-existing default agent config instead.
3. Sends a real prompt containing a distinctive, per-run marker token,
   triggers a real run.
4. **While that run is in flight** (concurrently, from a second thread --
   see the test's own comment on why this can't be a sequential
   post-hoc check), walks the real OS process tree under the
   agent-server's own pid and asserts a real `acpx-server` process, with
   a real `claude-agent-acp`/`codex-acp` process transitively underneath
   it, is actually running -- full lifecycle, not a black box (see
   `proc_tree.py`).
5. Waits for the real run to finish via the SDK's own real WebSocket-
   based completion detection (`/sockets/events/{id}`, the same wire
   protocol OpenHands's own frontend uses).
6. Fetches the real final response text (`GET /api/conversations/{id}/
   agent_final_response`) and asserts the marker token is actually
   present in it -- proves a real model reply came back through the
   whole chain, not just that the run reported "finished".

## Why attach to a running stack instead of spawning one

The OpenHands agent-server itself has its own heavyweight startup
(workspace/runtime provisioning, its own automation service, the
agent-canvas frontend) that's entirely orthogonal to what this suite is
actually testing (the acpx integration point). Requiring an
already-running stack keeps this suite fast and focused, at the cost of
not being fully self-contained CI-wise -- an operator (or a CI job that
already manages an OpenHands stack lifecycle) is expected to have one up
first. `conftest.py`'s skip-not-fail fixtures are what make that an
ergonomic tradeoff rather than a footgun.

## Full coverage: terminal, profiles, approval flow, multi-session

`test_openhands_acpx_e2e.py` (above) proves the base chat round trip.
Four more files round out full HTTP-surface coverage against the same
real, already-running agent-server, each independently runnable:

- `test_openhands_terminal.py` -- the host-level `/api/bash/*` terminal
  surface (synchronous `execute_bash_command`, async `start_bash_command`
  + poll, a non-zero exit code, and a real acpx-backed conversation
  running *concurrently* with terminal traffic to prove the two surfaces
  don't contend for a shared lock).
- `test_openhands_profiles.py` -- the `/api/agent-profiles/*` launch-spec
  profile store (not just `/api/profiles`, the LLM-config store): full
  save/get/activate/materialize CRUD round trip for an `ACPAgentProfile`
  pointed at each acpx wrapper script (both backends), plus a
  from-a-stored-profile conversation that launches the real backend
  process tree exactly like `test_openhands_acpx_e2e.py`'s direct
  `ACPAgent(...)` construction does.
- `test_openhands_approval_flow.py` -- `/api/conversations/{id}/
  confirmation_policy` and `/api/conversations/{id}/events/
  respond_to_confirmation`. Documents and asserts on a real,
  live-verified architectural finding: `ACPAgentClient.request_permission`
  auto-approves every ACP `session/request_permission` call, so the
  OpenHands-side confirmation-policy pause is a no-op for ACP-backed
  conversations -- see that file's module docstring for the full
  explanation and why the tests are written to assert on *actual*
  observed behavior rather than the behavior the endpoint names might
  suggest. Also covers `/api/conversations/{id}/ask_agent` (which
  internally forks the ACP session -- see the "real bugs found and fixed"
  section below).
- `test_openhands_multi_session.py` -- two real, concurrently-running
  acpx-backed conversations against the same agent-server: no
  cross-talk between their marker-token responses, distinct process
  trees (best-effort, see the `ps`-snapshot-stability note below), and
  `GET /api/conversations/search` correctly listing both independently.

Run the whole suite (still needs the prerequisites above -- a running
agent-server, a release `acpx-server` binary, real credentials):

```sh
uv run --with openhands-sdk==1.29.0 --with pytest \
    pytest tests/openhands_integration -v
```

### Real ACP/acpx bugs this suite found and fixed

Writing `test_ask_agent_side_channel_works_on_acp_conversation` surfaced
a genuine acpx transport bug, not a test artifact: `OpenHands.ask_agent`
implements its side-channel question by issuing a real ACP `session/
fork` against the live session, then a `session/prompt` against the
*newly forked* session id. acpx's `session/fork` dispatch logic (added
in ACP-compatibility phase 28/29) was correct, but the stdio/WS
transports' live-notification watch-subscription wiring
(`acpx-server/src/transport/live.rs::session_id_to_watch`) special-cased
only `session/new`'s freshly-minted `result.sessionId` -- not `session/
fork`'s equally freshly-minted one -- so after a fork the transport kept
watching the *source* session instead of the new forked one. The forked
session's first `session/prompt` call then had no live subscriber in
place for the router's `try_deliver_live` to find, so its `session/
update` notifications silently fell back to the `_acpx.updates` buffer
embedded in the JSON-RPC response -- a proprietary acpx extension no
standard ACP client (including OpenHands's `ACPAgentClient`) reads,
manifesting as `ask_agent` returning an empty string despite the backend
replying normally. Fixed by special-casing `session/fork` in
`session_id_to_watch` exactly like `session/new`; see `acpx/COVERAGE.md`
for the phase entry and `acpx-server/src/transport/live.rs`'s own tests
(`session_fork_watches_the_newly_minted_forked_gateway_id_not_the_
source_one`) for the regression coverage.

## A note on `ps` snapshot stability

`proc_tree.py` shells out to `ps -eo pid,ppid,args` rather than using
`/proc` directly or a third-party library (see `proc_tree.py`'s own doc
comment for the dependency-light rationale). In one particular sandboxed
coding-agent tool-execution environment used while building this suite,
a `ps` snapshot taken from *inside* a `pytest`-invoked process
consistently omitted a small, fixed set of real, still-running host
processes that the exact same `ps -eo pid,ppid,args` command found
correctly when run as a bare shell command, or as a bare `python3 -c
"..."` invocation, moments before and after -- repeatable across many
consecutive reads within the same process, so `proc_tree.snapshot_
stable()`'s consecutive-reads-agree retry (which does help with genuine
transient flakiness) does not paper over it. This looks like a
deliberate process-visibility isolation policy that specific tool
applies around test-runner invocations, not a bug in `ps`, in this
package's code, or in the acpx/OpenHands integration itself -- the same
assertions were independently confirmed correct via a manual, hand-
driven ACP session over the raw wrapper script earlier in that session
(a real Claude reply came back through the full stdio chain). If a
process-tree assertion in this suite ever fails in a *normal* shell/CI
environment (not from inside some other tool's own sandboxed test
runner) treat it as a real regression; if it fails only from inside such
a sandbox, check whether a bare, non-pytest script in that same sandbox
can see the target process at all before assuming acpx/OpenHands is at
fault.
