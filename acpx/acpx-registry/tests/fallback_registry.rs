//! Confirms the bundled `registry.fallback.json` (copied verbatim from
//! `memory/acpx/gen/plans/registry.fallback.json` into this crate so
//! `include_str!` has a stable in-crate path) parses with the same
//! `Registry`/`Agent`/`Distribution` types used for the live fetch, and
//! that all three "big three" agents round-trip with the expected
//! `npx`-preferred distribution per `01-research.md`.

use acpx_registry::fallback_registry;

#[test]
fn fallback_registry_parses_and_contains_the_big_three() {
    let registry = fallback_registry();
    assert_eq!(registry.version, "1.0.0");

    let ids: Vec<&str> = registry.agents.iter().map(|a| a.id.as_str()).collect();
    assert!(ids.contains(&"claude-acp"));
    assert!(ids.contains(&"codex-acp"));
    assert!(ids.contains(&"gemini"));

    for agent in &registry.agents {
        assert_eq!(
            agent.distribution.preferred_method(),
            Some("npx"),
            "agent {} expected to be npx-only per 01-research.md",
            agent.id
        );
    }
}
