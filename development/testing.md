# Slint MCP end-to-end tests

`panel-rust` embeds Slint's own MCP UI-testing server (`SLINT_MCP_PORT`), which
lets a test drive the real compiled UI headlessly over HTTP JSON-RPC instead
of raw screen/VNC coordinates. Full harness (Xvfb + real `acpx-server`,
mock-backed + the real compiled `snapflow`/`shotcut` binary):
`panel-rust/tests/slint_mcp_acp_provider_matrix_e2e_test.rs`.

Run it:

```bash
cd panel-rust
cargo test --test slint_mcp_acp_provider_matrix_e2e_test -- --test-threads=1
```

Minimal example, in that file's own code (`LiveUiHarness::tool_call` +
one full test):

```rust
async fn tool_call(&self, name: &str, arguments: Value) -> Value {
    let resp = self
        .mcp_call("tools/call", json!({"name": name, "arguments": arguments}))
        .await;
    let result = resp
        .get("result")
        .unwrap_or_else(|| panic!("tool {name} returned no result: {resp}"));
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool {name} result missing content[0].text: {result}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool {name} result not JSON ({e}): {text}"))
}

#[tokio::test]
async fn provider_matrix_codex_acp_selects_and_sends_without_crashing() {
    let harness = LiveUiHarness::spawn().await;
    let window = harness.window_handle().await;
    harness.select_provider_for_new_thread(&window, "codex-acp").await;
    harness.send_and_expect_a_reply(&window, "diagnostic ping via codex-acp").await;
}
```

`select_provider_for_new_thread`/`send_and_expect_a_reply` are `tool_call`
wrappers around `list_windows`, `get_element_tree` (find by
`accessibleLabel`), `click_element`/`invoke_accessibility_action`, and
`dispatch_key_event` -- see the same file for those and for
`LiveUiHarness::spawn` (the Xvfb/acpx-server/snapflow process wiring).
