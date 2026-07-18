---
doc: acpx/docs/development
part_of: acpx
status: living
---

# Development

Building, testing, formatting/linting, and this codebase's own testing
conventions. See [`setup.md`](./setup.md) for running a built daemon,
and [`architecture.md`](./architecture.md) for how it's put together.

## Build

```sh
cd acpx
cargo build --workspace            # debug
cargo build --workspace --release  # optimized, what the release scripts/CI artifacts use
```

## Test layers

`cargo test --workspace` is the primary, always-run correctness gate --
every layer below except the explicitly-marked opt-in ones is part of
it. Matches `.github/workflows/ci.yml`'s `test` job exactly (no extra
flags).

```sh
cargo test --workspace
```

| Layer | What it exercises | Real cost/network? |
| --- | --- | --- |
| Unit tests (`src/**/*.rs` `#[cfg(test)]` modules) | Pure logic: config validation, session registry bookkeeping, schema building, etc. | No |
| Integration tests (`*/tests/*.rs`) | Real `sh`-scripted stand-in backends, real `Router`/`Supervisor`, real sqlite files, real HTTP/WS round trips over ephemeral ports | No (synthetic backends only) |
| `acpx-registry/tests/binary_install_real_download_test.rs` | A real download+extract against a same-machine loopback HTTP server | No external network (loopback only) |
| `acpx-registry/tests/live_registry.rs` | The real upstream ACP registry endpoint | Yes -- `#[ignore]`d, CI's `live_registry_check` schedule-only job runs it with `-- --ignored` |
| `acpx-core/tests/real_claude_terminal_capability_probe.rs` and similar `real_*` tests under `acpx-server/tests/` | A real `claude-agent-acp`/`codex-acp` process against a real, already-authenticated model | Yes -- real API cost, `#[ignore]`d, run explicitly |
| `tests/bridge_integration/`, `tests/recovery_integration/` (Python, run from `acpx/`) | Black-box daemon-restart/bridge behavior against a real built `acpx-server` binary and synthetic stand-in backends | No (synthetic backends only); `tests/recovery_integration/acp_ws_client.py` needs `pip install -r tests/recovery_integration/requirements.txt` |
| `tests/openhands_integration/` (Python, `uv run --with openhands-sdk==<version> --with pytest`) | A real, already-running OpenHands agent-server driving real Claude/Codex conversations through acpx | Yes -- real API cost, real credentials, real agent-server process; see that directory's own `README.md` |

Run one crate/test file at a time while iterating (the full workspace
build+test cycle is slow):

```sh
cargo test -p acpx-core --lib
cargo test -p acpx-core --test session_process_isolation_test
cargo test -p acpx-server --test admin_test
```

## Formatting and linting

CI (`fmt`/`clippy` jobs) enforces both on every push/PR:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Black-box smoke test

`scripts/self_test.sh` builds the workspace, boots a real `acpx-server`
against a trivial synthetic stand-in backend, and runs the
`acpx-selftest` CLI against it over real HTTP -- a fast way to confirm a
fresh build actually works end-to-end without any real backend agent or
API key:

```sh
cd acpx
./scripts/self_test.sh
```

## Regenerating the schema artifacts

See [`schema/README.md`](./schema/README.md) for what each document is;
regenerate all three plus their drift-guard tests after any change to
`acpx-proto/src/{openrpc,openapi,schema}.rs` or the dispatched method
table:

```sh
bash scripts/gen_schema.sh
bash scripts/gen_openrpc.sh
bash scripts/gen_openapi.sh
cargo test -p acpx-proto
```

## Conventions this codebase's history has established

- **No plan/phase/agent/co-author references in commit subjects** --
  commit messages describe the feature/behavior change itself.
- **Real bugs found while implementing something else get fixed and
  called out**, not silently folded in or deferred -- see recent commit
  bodies and `COVERAGE.md` for examples of this pattern (a fix found
  while auditing an unrelated gap gets its own regression test and an
  explicit doc comment explaining what broke and why).
- **Every schema/spec artifact is generated, never hand-edited** -- see
  [`schema/README.md`](./schema/README.md)'s drift-guard test
  description.
- **`#[ignore]`d tests are real, runnable tests, not placeholders** --
  every one hits a real network resource or a real paid API and is
  documented as such (see the test-layer table above), never disabled
  because it doesn't pass.
