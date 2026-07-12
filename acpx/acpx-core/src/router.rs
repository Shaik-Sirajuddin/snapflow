//! Method classification (gateway-native vs. proxied vs. hybrid) per
//! `02-architecture.md`'s classification table. Phase 1 only needs
//! classification for the single-agent passthrough set; profile
//! resolution, MCP-server merge, and gateway-native handlers land in
//! Phase 2/3.

/// Which bucket a given JSON-RPC method falls into. See the classification
/// table in `02-architecture.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Handled entirely in-process; no backend agent involved.
    GatewayNative,
    /// Session-resolve + forward, payload untouched.
    Proxied,
    /// One-time gateway logic (profile/agent resolution + spawn), then
    /// delegates to the backend.
    Hybrid,
    /// Not a recognized ACP or acpx method.
    Unknown,
}

/// Classify a JSON-RPC method name. Pure function, no state -- routing
/// state (session registry, profile store, conductor) lives in `Router`.
pub fn classify(method: &str) -> MethodClass {
    match method {
        "session/new" => MethodClass::Hybrid,
        "session/prompt" | "session/resume" | "session/load" | "session/close"
        | "session/set_mode" | "session/cancel" => MethodClass::Proxied,
        "agents/list" | "agents/install" | "agents/status" | "session/list" => {
            MethodClass::GatewayNative
        }
        "profiles/create" | "profiles/list" | "profiles/update" | "profiles/delete" => {
            MethodClass::GatewayNative
        }
        _ => MethodClass::Unknown,
    }
}

/// Phase 1 stub: the real `Router` composes `SessionRegistry` +
/// `acpx-conductor::Supervisor` + (from Phase 3) `ProfileStore` to actually
/// dispatch. Left unimplemented here; `acpx-server`'s Phase 1 spike talks to
/// `acpx-conductor` directly for its single hardcoded backend instead of
/// going through this type, per `04-phased-plan.md` step 4's "validates the
/// framing/spawn/proxy plumbing in isolation before adding gateway
/// complexity" note.
pub struct Router;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_methods() {
        assert_eq!(classify("session/new"), MethodClass::Hybrid);
        assert_eq!(classify("session/prompt"), MethodClass::Proxied);
        assert_eq!(classify("agents/list"), MethodClass::GatewayNative);
        assert_eq!(classify("bogus/method"), MethodClass::Unknown);
    }
}
