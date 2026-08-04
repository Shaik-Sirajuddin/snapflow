//! Central named MCP server registry, merged by name into native
//! `mcpServers` at `session/new` (client entries win on collision). Phase 3
//! step 17a.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// True when a central-registry MCP entry should be attached at
/// `session/new` / resume / load. Missing `enabled` means enabled
/// (legacy rows written before the field existed). Explicit
/// `enabled: false` is the Settings toggle — those must not reach the
/// backend agent even if the profile still lists the name.
pub fn mcp_entry_is_enabled(entry: &Value) -> bool {
    entry
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Merge the client's own `mcpServers` array with the profile's centrally
/// configured servers, keyed by `name`. Client entries win on collision --
/// see `02-architecture.md`'s "must stay strictly additive" rule. An empty
/// `central` set makes this a no-op, so a client using no acpx extensions
/// gets plain native ACP behavior unaffected by this store's existence.
///
/// **Disabled central entries are skipped.** Settings' enable toggle only
/// flips `enabled` on the store row; without this filter a pool-acquired
/// session would keep receiving `enabled: false` servers forever.
pub fn merge_mcp_servers(client: &[Value], central: &[Value]) -> Vec<Value> {
    let mut by_name: HashMap<String, Value> = HashMap::new();
    for entry in central {
        if !mcp_entry_is_enabled(entry) {
            continue;
        }
        if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
            by_name.insert(name.to_string(), entry.clone());
        }
    }
    for entry in client {
        if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
            by_name.insert(name.to_string(), entry.clone()); // client wins
        }
    }
    by_name.into_values().collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpServerStoreError {
    #[error("mcp server {0} already exists")]
    AlreadyExists(String),
    #[error("no mcp server named {0}")]
    NotFound(String),
}

/// CRUD store for the centrally-registered servers `merge_mcp_servers`
/// draws its `central` argument from. Each entry is kept as a raw
/// `serde_json::Value` (the same shape ACP's own `mcpServers` array
/// elements use) rather than a typed struct -- `acpx` never interprets an
/// MCP server entry's fields itself, it only ever passes them through to
/// the backend agent, so re-typing them here would just be a second place
/// to keep in sync with ACP's schema. `create`/`update` both require a
/// `"name"` string field (the merge key); anything else is opaque.
///
/// Backed by `Arc<Mutex<..>>` (not a plain `HashMap`) and cheaply `Clone`
/// so a detached background task -- specifically `Router::
/// authenticate_mcp_server`'s OAuth completion, which finishes on its own
/// time after the loopback redirect arrives, well after the `&mut Router`
/// borrow from the original `mcp_servers/authenticate` RPC call has
/// ended -- can hold its own handle and patch an entry's `auth_status`
/// in-place without needing access back to the `Router` that created it.
/// All methods therefore take `&self`, mirroring `PersistenceStore`'s own
/// interior-mutability shape for the same reason.
#[derive(Debug, Default, Clone)]
pub struct McpServerStore {
    servers: Arc<Mutex<HashMap<String, Value>>>,
}

impl McpServerStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn name_of(entry: &Value) -> Result<String, McpServerStoreError> {
        entry
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string)
            .ok_or_else(|| McpServerStoreError::NotFound("<missing \"name\">".to_string()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Value>> {
        self.servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn create(&self, entry: Value) -> Result<(), McpServerStoreError> {
        let name = Self::name_of(&entry)?;
        let mut servers = self.lock();
        if servers.contains_key(&name) {
            return Err(McpServerStoreError::AlreadyExists(name));
        }
        servers.insert(name, entry);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.lock().get(name).cloned()
    }

    /// All entries, in the shape `merge_mcp_servers`'s `central` parameter
    /// expects.
    pub fn list(&self) -> Vec<Value> {
        self.lock().values().cloned().collect()
    }

    /// Entries for exactly the given names -- what a
    /// `Profile::mcp_servers` name list resolves to at `session/new`.
    /// Disabled (`enabled: false`) rows are omitted so a Settings toggle
    /// off is not silently re-attached by a still-listed profile name.
    pub fn list_named(&self, names: &[String]) -> Vec<Value> {
        let servers = self.lock();
        names
            .iter()
            .filter_map(|name| servers.get(name).cloned())
            .filter(mcp_entry_is_enabled)
            .collect()
    }

    pub fn update(&self, entry: Value) -> Result<(), McpServerStoreError> {
        let name = Self::name_of(&entry)?;
        let mut servers = self.lock();
        if !servers.contains_key(&name) {
            return Err(McpServerStoreError::NotFound(name));
        }
        servers.insert(name, entry);
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<(), McpServerStoreError> {
        self.lock()
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| McpServerStoreError::NotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_central_set_is_a_no_op() {
        let client = vec![json!({"name": "fs", "command": "mcp-fs"})];
        let merged = merge_mcp_servers(&client, &[]);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn client_entry_wins_on_name_collision() {
        let client = vec![json!({"name": "fs", "command": "client-fs"})];
        let central = vec![json!({"name": "fs", "command": "central-fs"})];
        let merged = merge_mcp_servers(&client, &central);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["command"], "client-fs");
    }

    fn fs_entry() -> Value {
        json!({"name": "fs", "command": "mcp-fs"})
    }

    #[test]
    fn store_create_then_get_round_trips() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        assert_eq!(store.get("fs").unwrap()["command"], "mcp-fs");
    }

    #[test]
    fn store_create_twice_errors() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        assert_eq!(
            store.create(fs_entry()),
            Err(McpServerStoreError::AlreadyExists("fs".to_string()))
        );
    }

    #[test]
    fn store_create_without_name_errors() {
        let store = McpServerStore::new();
        assert!(store.create(json!({"command": "no-name"})).is_err());
    }

    #[test]
    fn store_update_missing_errors() {
        let store = McpServerStore::new();
        assert_eq!(
            store.update(fs_entry()),
            Err(McpServerStoreError::NotFound("fs".to_string()))
        );
    }

    #[test]
    fn store_delete_then_get_returns_none() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        store.delete("fs").unwrap();
        assert!(store.get("fs").is_none());
    }

    #[test]
    fn store_list_named_filters_and_preserves_order() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        store
            .create(json!({"name": "git", "command": "mcp-git"}))
            .unwrap();
        let named = store.list_named(&["git".to_string(), "does-not-exist".to_string()]);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0]["name"], "git");
    }

    #[test]
    fn store_list_and_merge_mcp_servers_compose() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        let client = vec![json!({"name": "git", "command": "client-git"})];
        let merged = merge_mcp_servers(&client, &store.list());
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_skips_disabled_central_entries() {
        let client = vec![json!({"name": "git", "command": "client-git"})];
        let central = vec![
            json!({"name": "fs", "command": "mcp-fs", "enabled": false}),
            json!({"name": "httpbin", "url": "https://example", "enabled": true}),
        ];
        let merged = merge_mcp_servers(&client, &central);
        let names: Vec<_> = merged
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"git"));
        assert!(names.contains(&"httpbin"));
        assert!(
            !names.contains(&"fs"),
            "enabled:false central must not reach session mcpServers"
        );
    }

    #[test]
    fn list_named_omits_disabled() {
        let store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        store
            .create(json!({"name": "off", "command": "x", "enabled": false}))
            .unwrap();
        let named = store.list_named(&["fs".into(), "off".into()]);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0]["name"], "fs");
    }
}
