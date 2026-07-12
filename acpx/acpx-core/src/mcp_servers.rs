//! Central named MCP server registry, merged by name into native
//! `mcpServers` at `session/new` (client entries win on collision). Phase 3
//! step 17a -- stub for now.

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
}
