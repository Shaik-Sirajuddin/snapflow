//! TTL cache for capability probes.
//!
//! A probe is allowed to create one disposable ACP session, but catalog
//! endpoints must never trigger a new backend process for every request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::AdapterCapabilities;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityCacheKey {
    pub adapter_id: String,
    pub adapter_version: Option<String>,
}

impl CapabilityCacheKey {
    pub fn new(adapter_id: impl Into<String>, adapter_version: Option<String>) -> Self {
        Self {
            adapter_id: adapter_id.into(),
            adapter_version,
        }
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    capabilities: AdapterCapabilities,
    expires_at: Instant,
}

#[derive(Debug)]
pub struct CapabilityCache {
    ttl: Duration,
    entries: HashMap<CapabilityCacheKey, CacheEntry>,
}

impl CapabilityCache {
    pub fn new(ttl: Duration) -> Self {
        assert!(!ttl.is_zero(), "capability cache TTL must be non-zero");
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn get(&mut self, key: &CapabilityCacheKey, now: Instant) -> Option<AdapterCapabilities> {
        let entry = self.entries.get(key)?;
        if entry.expires_at > now {
            return Some(entry.capabilities.clone());
        }
        self.entries.remove(key);
        None
    }

    pub fn put(
        &mut self,
        key: CapabilityCacheKey,
        capabilities: AdapterCapabilities,
        now: Instant,
    ) {
        self.entries.insert(
            key,
            CacheEntry {
                capabilities,
                expires_at: now + self.ttl,
            },
        );
    }

    /// Removes old-version and stale entries for one adapter before a fresh
    /// probe result is stored. A newly installed adapter version must not
    /// inherit model or permission choices from the previous binary.
    pub fn invalidate_adapter(&mut self, adapter_id: &str) {
        self.entries.retain(|key, _| key.adapter_id != adapter_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdapterCapabilities;

    fn capabilities(version: &str) -> AdapterCapabilities {
        AdapterCapabilities {
            adapter_id: "claude-acp".to_string(),
            adapter_version: Some(version.to_string()),
            models: vec![],
            permission_modes: vec![],
            config_options: vec![],
            auth_methods: vec![],
        }
    }

    #[test]
    fn expires_entries_and_keeps_versions_isolated() {
        let start = Instant::now();
        let mut cache = CapabilityCache::new(Duration::from_secs(60));
        let v1 = CapabilityCacheKey::new("claude-acp", Some("0.58.0".to_string()));
        let v2 = CapabilityCacheKey::new("claude-acp", Some("0.59.0".to_string()));
        cache.put(v1.clone(), capabilities("0.58.0"), start);

        assert!(cache.get(&v1, start + Duration::from_secs(59)).is_some());
        assert!(cache.get(&v2, start + Duration::from_secs(59)).is_none());
        assert!(cache.get(&v1, start + Duration::from_secs(60)).is_none());
    }

    #[test]
    fn invalidating_adapter_removes_all_versions() {
        let start = Instant::now();
        let mut cache = CapabilityCache::new(Duration::from_secs(60));
        let claude = CapabilityCacheKey::new("claude-acp", Some("0.59.0".to_string()));
        let codex = CapabilityCacheKey::new("codex-acp", Some("1.1.2".to_string()));
        cache.put(claude.clone(), capabilities("0.59.0"), start);
        cache.put(codex.clone(), capabilities("1.1.2"), start);

        cache.invalidate_adapter("claude-acp");
        assert!(cache.get(&claude, start).is_none());
        assert!(cache.get(&codex, start).is_some());
    }
}
