# ACPX black-box recovery protocol tests

Synthetic (no real LLM adapter, no network dependency, no credentials)
black-box coverage for ACPX's durable session-recovery contract, driven
entirely against a real `acpx-server` binary and a real stand-in shell
backend -- see `../../memory/acpx/gen/plans/acpx-session-recovery/
02-phased-plan.md`'s test asset table for how this package fits
alongside the Rust integration suite (`acpx-server/tests/
startup_session_recovery_e2e_test.rs`) and the real-adapter OpenHands
suite (`../openhands_integration/`).

## Modules

| Module | Responsibility |
| --- | --- |
| `acp_http_client.py` | Dependency-free (stdlib-only) JSON-RPC client for `/rpc` and `/health` |
| `acp_ws_client.py` | JSON-RPC client for `/ws` that records request/response/live-update ordering and supports `_acpx.resume` reconnect cursors |
| `proc_tree.py` | Dependency-free `ps`-based process-tree snapshotting, used to prove real connector process reuse/restart/exit rather than trusting the JSON-RPC-level response alone |
| `test_recovery_protocol.py` | HTTP-only daemon-restart recovery scenarios |
| `test_recovery_ws.py` | Same daemon-restart recovery scenario over `/ws`, plus real process-tree assertions that the pre-restart backend connector process actually exits (no leak) and the post-restart one is a genuinely distinct process (no accidental pid reuse/staleness) |

## Running

Build the debug binary first (every test here `self.skipTest`s cleanly
if it's missing):

```
cd .. && cargo build -p acpx-server
```

`acp_ws_client.py` (and therefore `test_recovery_ws.py`) additionally
needs the third-party `websockets` package -- see
`requirements.txt` in this directory. `acp_http_client.py`,
`proc_tree.py`, and `test_recovery_protocol.py` are stdlib-only and need
nothing beyond a Python 3.11+ interpreter.

```
pip install -r requirements.txt
python3 -m unittest discover -s tests/recovery_integration -t .
```

(run from `acpx/`, matching `test_recovery_protocol.py`'s own module
docstring -- `-t .` puts `acpx/` itself on `sys.path` so `tests.
recovery_integration.*` imports resolve without a `tests/__init__.py`).
