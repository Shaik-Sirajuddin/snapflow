# ACPX skills/settings verification suite (designa v2)

Real end-to-end coverage for `memory/designa/tasks/v2/init.yaml`'s
`testing`/`testing-a`/`local-llm`/`llm-edge`/`ui-state testing`/
`runtime-testing`/`acpx-verify` task blocks -- see
`memory/designa/gen/plans/skills-settings-e2e-verification/` for the full
plan this suite implements. Pytest-fixture style (`openhands_integration`'s
convention in this `tests/` directory), reusing `recovery_integration`'s
stdlib-only `AcpHttpClient` and its build-then-spawn-a-real-`acpx-server`
pattern rather than a third client implementation.

## Modules

| Module | Responsibility |
| --- | --- |
| `conftest.py` | `acpx_server_binary`/`acpx_server` fixtures: finds a built `acpx-server` (release or debug), spawns it against a synthetic backend, skips cleanly (not a hard failure) if no binary is built yet |
| `test_harness_smoke.py` | Proves the fixture itself works end-to-end against a real spawned server -- the actual skill/settings tests below build on this rather than re-deriving server-spawning logic |

Tests still to add here, per the phased plan: `test_settings_reflection.py`,
`test_skill_injection.py` (blocked on `skill-manager-workspace`'s
`skill_discovery_backend` phase landing), `test_model_runtime_match.py`.

## Running

Build the binary first (release or debug, either is found automatically):

```
cd .. && cargo build --release -p acpx-server
```

```
python3 -m pytest tests/skills_settings_integration -v
```

(run from `acpx/` -- this package's `conftest.py` imports
`tests.recovery_integration.acp_http_client` as a sibling package, which
needs `acpx/tests/__init__.py` to exist for pytest's import resolution;
that file was added alongside this suite and does not affect
`recovery_integration`'s own `unittest` invocation.)
