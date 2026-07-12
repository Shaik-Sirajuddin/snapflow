//! Manual-verification-only test that hits the real, live ACP registry
//! endpoint. `#[ignore]`d so `cargo test --workspace` stays hermetic (see
//! 04-phased-plan.md step 18 / this crate's requirement that only
//! `fetch_registry`/`fetch_registry_or_fallback` need network, and even
//! those must not be required for the crate to build/test). Run explicitly
//! with `cargo test -p acpx-registry --test live_registry -- --ignored`.

use acpx_registry::fetch_registry;

#[tokio::test]
#[ignore = "hits the real network; run manually with `-- --ignored`"]
async fn live_registry_matches_expected_shape() {
    let client = reqwest::Client::new();
    let registry = fetch_registry(&client)
        .await
        .expect("live registry endpoint should be reachable and parse");
    assert!(!registry.agents.is_empty());
    assert!(registry
        .agents
        .iter()
        .any(|a| a.id == "codex-acp" || a.id == "claude-acp"));
}
