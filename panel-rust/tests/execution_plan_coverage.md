# Chat Panel Execution Coverage

This is the executable-test checklist for
`memory/rui/gen/plans/chat-panel-production-ui/execution-plan.md`.

Automated host checks use X11/XTEST input, the opt-in
`RUI_PANEL_INPUT_TRACE` host trace, and mock-agent JSONL evidence. They never
capture screenshots or use VNC. VNC remains a manual-only inspection option.

Before running panel gateway integration tests, rebuild the real binary they
launch directly:

```bash
(cd acpx && cargo build --bin acpx-server)
(cd panel-rust && cargo test --tests -- --test-threads=1)
```

`panel-rust`'s Cargo test graph does not own `acpx-server`, so it cannot
automatically rebuild that sibling executable.

| Coverage row | Current direct evidence | Status |
| --- | --- | --- |
| `session/new`, `session/prompt`, `session/load`, `session/resume` | `gateway_actor_e2e_test`, `host_e2e_smoke.sh` restart scenario | Proven |
| `session/cancel` | `agent_bridge` real slow-turn test (bash stand-in backend), `cancel_session_ends_a_real_mock_agent_slow_turn_as_cancelled` (real gateway, real compiled `rui-mock-agent` binary), and `PANEL_HOST_E2E_CANCEL=1` (real Shotcut: XTEST click on the Send/Stop toggle, backend `session/cancel` record, host trace `turn ended thread=0 reason="cancelled"`) | Proven at gateway and host layers |
| `session/set_mode`, `session/set_config_option` | `agent_bridge` capability E2E and `slint_component_e2e_test` typed callbacks | Proven at gateway/component layers |
| `session/update` variants (thought/tool/message discriminators) | Gateway/component tests plus `PANEL_HOST_E2E_TOOL_STREAM=1` (real Shotcut: host trace of the typed reducer transcript's own tail shows the exact `thinking`/`tool-call`/`agent` entries a real turn produces) | Proven at gateway, component, and host layers |
| `session/request_permission`, FS, terminal relay | `gateway_actor_terminal_relay_e2e_test`, component approval callbacks, and `a_foreign_connections_forged_relay_response_is_rejected_and_the_real_owner_still_answers` (real `acpx-server`, proves a never-subscribed connection's forged relay response is rejected while the real owner's own answer still lands) | Gateway/component proven, including the owner-vs-foreign-client contract; host UI click scenario investigated at length but not landed, see `host_e2e_matrix.md`'s permission-approval note |
| agent terminal output and local PTY | terminal relay E2E, local terminal E2E, component focus/key test, `PANEL_HOST_E2E_LOCAL_TERMINAL=1` (real Shotcut: open/type/echo/close round trip against a genuine PTY -- real shell prompt observed, real echoed command output) | Gateway/component proven; client-PTY host scenario proven single-threaded, two-terminals-in-parallel and agent-created terminal's own host scenario still pending |
| `profiles/list/create/update/delete` | `profiles_crud_round_trips_through_the_thread_actor` (real gateway), `profile_referencing_a_central_mcp_server_reaches_the_real_backend_session_new` (session-creation attach), settings-sheet chip picker + inline remove/confirm/cancel + add-profile form (`slint_component_e2e_test`) | Proven at gateway and component layers; host scenario pending |
| MCP servers and agent catalog | `gateway_actor_mcp_agents_e2e_test`; component tests cover create/delete/install callback payloads | Host scenario pending |
| transcript tail paging | JSONL paging unit tests and accessible Load Older action | Host scroll-boundary evidence pending |
| host appearance | `lib.rs` appearance preservation test | Real-host callback scenario pending |
| HTTP degraded mode | SDK transport tests and visible `HTTP fallback - approvals unavailable` connection state | Host recovery scenario pending |
| `initialize`, `authenticate`, `logout` | `transport_status_reports_live_connection_after_a_real_websocket_attach` (real gateway, `initialize` proven via a live WS `session/new` round trip) plus the pre-existing `connection-status` accessibility test | Proven -- see note below |
| `session/list` recovery/import | `AgentBridge::recoverable_sessions`/`add_thread_recovering_session`, real gateway test `recoverable_sessions_lists_the_orphan_and_attaching_it_replays_its_real_history`, settings-sheet "Recoverable Sessions" list + Attach control (`slint_component_e2e_test`) | Proven at gateway and component layers; host scenario pending |
| explicit `session/close`, `session/delete` | `close_then_delete_session_round_trip_through_a_real_gateway` (real gateway, backend-event-log proof of both real requests, session/list eviction proof), sidebar two-step arm/confirm close/delete controls (`sidebar_thread_close_and_delete_controls_are_addressable_and_two_step_confirmed`) | Proven at gateway and component layers; host scenario pending |
| provider isolation and parallel sessions | gateway actor integration tests, `PANEL_HOST_E2E_PROVIDER_ISOLATION=1` (real Shotcut, distinct `session/prompt` records for the fixture's default Codex/Claude threads) | Provider isolation proven at host layer; concurrent/parallel-turn variant still pending |
| new thread (two threads, one provider) | `PANEL_HOST_E2E_NEW_THREAD=1` (real Shotcut: XTEST click on the sidebar "New thread" control, a genuinely new `session/new` record, and a prompt bound to it distinct from thread 0's session) | Proven at host layer |

The `host_e2e_matrix.md` rows describe required host scenarios. A row must not
be considered complete until its XTEST driver produces its listed backend or
host-trace evidence in a real Shotcut process.

**Note on `initialize`/`authenticate`/`logout`:** verified directly against
`acpx-core::router` (`dispatch_native`'s `"authenticate"`/`"logout"` arms,
2026-07-16) before marking this row Proven, not assumed from the method
names alone. acpx's own `initialize` response always advertises
`"authMethods": []` and never sets `agentCapabilities.auth.logout` --
deliberate router behavior (acpx's access control is transport-level
HTTP-bearer/WS auth, not ACP-level session auth). A spec-compliant client
only calls `authenticate`/`logout` when the corresponding capability is
advertised; since acpx never advertises either, there is no real
login/logout UI state for the panel to build without misrepresenting a
capability this gateway does not have. The panel's real, meaningful
connection/auth surface is `AgentBridge::transport_status`'s three states
(`Connecting...`/`Live connection`/`HTTP fallback - approvals unavailable`),
which is what this row's test actually exercises.
