//! Central named MCP server registry, merged by name into native
//! `mcpServers` at `session/new` (client entries win on collision). Phase 3
//! step 17a.

use serde_json::Value;
use std::collections::HashMap;

/// Merge the client's own `mcpServers` array with the profile's centrally
/// configured servers, keyed by `name`. Client entries win on collision --
/// see `02-architecture.md`'s "must stay strictly additive" rule. An empty
/// `central` set makes this a no-op, so a client using no acpx extensions
/// gets plain native ACP behavior unaffected by this store's existence.
pub fn merge_mcp_servers(client: &[Value], central: &[Value]) -> Vec<Value> {
    let mut by_name: HashMap<String, Value> = HashMap::new();
    for entry in central {
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
#[derive(Debug, Default)]
pub struct McpServerStore {
    servers: HashMap<String, Value>,
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

    pub fn create(&mut self, entry: Value) -> Result<(), McpServerStoreError> {
        let name = Self::name_of(&entry)?;
        if self.servers.contains_key(&name) {
            return Err(McpServerStoreError::AlreadyExists(name));
        }
        self.servers.insert(name, entry);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.servers.get(name)
    }

    /// All entries, in the shape `merge_mcp_servers`'s `central` parameter
    /// expects.
    pub fn list(&self) -> Vec<Value> {
        self.servers.values().cloned().collect()
    }

    /// Entries for exactly the given names -- what a
    /// `Profile::mcp_servers` name list resolves to at `session/new`.
    pub fn list_named(&self, names: &[String]) -> Vec<Value> {
        names
            .iter()
            .filter_map(|name| self.servers.get(name).cloned())
            .collect()
    }

    pub fn update(&mut self, entry: Value) -> Result<(), McpServerStoreError> {
        let name = Self::name_of(&entry)?;
        if !self.servers.contains_key(&name) {
            return Err(McpServerStoreError::NotFound(name));
        }
        self.servers.insert(name, entry);
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), McpServerStoreError> {
        self.servers
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
        let mut store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        assert_eq!(store.get("fs").unwrap()["command"], "mcp-fs");
    }

    #[test]
    fn store_create_twice_errors() {
        let mut store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        assert_eq!(
            store.create(fs_entry()),
            Err(McpServerStoreError::AlreadyExists("fs".to_string()))
        );
    }

    #[test]
    fn store_create_without_name_errors() {
        let mut store = McpServerStore::new();
        assert!(store.create(json!({"command": "no-name"})).is_err());
    }

    #[test]
    fn store_update_missing_errors() {
        let mut store = McpServerStore::new();
        assert_eq!(
            store.update(fs_entry()),
            Err(McpServerStoreError::NotFound("fs".to_string()))
        );
    }

    #[test]
    fn store_delete_then_get_returns_none() {
        let mut store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        store.delete("fs").unwrap();
        assert!(store.get("fs").is_none());
    }

    #[test]
    fn store_list_named_filters_and_preserves_order() {
        let mut store = McpServerStore::new();
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
        let mut store = McpServerStore::new();
        store.create(fs_entry()).unwrap();
        let client = vec![json!({"name": "git", "command": "client-git"})];
        let merged = merge_mcp_servers(&client, &store.list());
        assert_eq!(merged.len(), 2);
    }
}
