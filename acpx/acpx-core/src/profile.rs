//! Profile store: CRUD for {agent, provider, key-ref, launch overrides}.
//! Phase 3 step 14 -- stub for now.

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub agent_id: String,
    pub provider: Option<String>,
}
