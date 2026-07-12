use acpx_registry::Registry;

#[test]
fn parses_trimmed_registry_fixture() {
    let json = r#"{
        "version": "1.0.0",
        "agents": [
            {
                "id": "codex-acp",
                "name": "Codex",
                "version": "1.1.2",
                "authors": [],
                "distribution": {
                    "npx": { "package": "@agentclientprotocol/codex-acp@1.1.2" }
                }
            }
        ],
        "extensions": []
    }"#;
    let registry: Registry = serde_json::from_str(json).unwrap();
    assert_eq!(registry.agents.len(), 1);
    assert_eq!(registry.agents[0].id, "codex-acp");
    assert_eq!(
        registry.agents[0].distribution.preferred_method(),
        Some("npx")
    );
}
