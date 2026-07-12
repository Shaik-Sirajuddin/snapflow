//! Provider config model: openai / anthropic / litellm endpoints + keys.
//! Phase 3 step 12 -- stub for now.

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
}
