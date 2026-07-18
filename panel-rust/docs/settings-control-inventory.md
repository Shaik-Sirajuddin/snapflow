# Settings control → store/RPC inventory (Phase 0)

| UI control | Callback / property | Store / RPC |
|------------|---------------------|-------------|
| Default profile | `default_profile` + save | JSON global/project (`settings_file`) |
| Permission profile | `permission_profile` + save | JSON only (option **A**: UI label) |
| Background default | `background_default` + save | JSON |
| Per-thread background | override props / toggle | SQLite `thread_settings` |
| Selected thread | `selected_thread` | SQLite `selected_thread_id` |
| Available profiles | settings open / create/delete | `profiles/list\|create\|delete` via `settings_gateway_index()` |
| MCP servers | settings open / create/delete | `mcp_servers/list\|create\|delete` via gateway index |
| Agent catalog | settings open / install | `agents/list\|install` via gateway index |
| Recoverable sessions | settings open / attach | `session/list` recovery on selected provider |
| Harness toggles | UI shell | JSON `harness.*` (load/save path reserved) |

## AgentBridge settings surface

- `list_profiles` / `create_profile` / `delete_profile`
- `list_mcp_servers` / `create_mcp_server` / `delete_mcp_server`
- `list_agents` / `install_agent`
- `add_thread_with_profile` / `recoverable_sessions`

## Golden fixtures (e2e)

See `gateway_actor_mcp_agents_e2e_test.rs` and
`snapshotd/scripts/e2e_acpx_panel.sh` for live list shapes.
