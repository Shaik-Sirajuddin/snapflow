//! Real JSON Schema *validation*, not just generation -- closes the gap
//! found when asked directly "did you verify this with codex exec or
//! claude e2e, does the daemon actually use/detect the strict schema?"
//! Every prior phase of the `acpx-openrpc-schema` plan only proved the
//! generated documents are internally consistent (drift guards, type-name
//! cross-checks against `router.rs`/upstream macro invocations) -- none
//! of it ever ran a real `jsonschema` validator against real JSON that
//! actually went over the wire to/from a real `claude-agent-acp`/
//! `codex-acp` backend process. This module is what makes that possible;
//! `acpx-server/tests/real_ambient_multi_agent_test.rs` is what actually
//! exercises it against real backends (see that file's
//! `assert_schema_valid` call sites, added alongside this module).

use serde_json::Value;

use crate::methods::{SchemaRef, METHODS};
use crate::schema::register_all_defs;
use schemars::generate::SchemaSettings;

fn format_errors(instance: &Value, type_name: &str, schema: &Value) -> Vec<String> {
    let validator = jsonschema::validator_for(schema).unwrap_or_else(|e| {
        panic!("BUG: acpx's own generated schema for {type_name} does not compile as a JSON Schema: {e}")
    });
    validator
        .iter_errors(instance)
        .map(|e| format!("{} (at {}): {}", type_name, e.instance_path(), e))
        .collect()
}

/// Validates `instance` against the acpx-generated schema for Rust type
/// `type_name` (must be a `$defs` key `register_all_defs` registers --
/// every [`SchemaRef`] name in `methods.rs`'s [`METHODS`] table qualifies,
/// see `schema.rs`'s `every_method_schema_ref_is_registered` test for the
/// guarantee that every such name really is registered). Returns every
/// validation failure found (empty means valid), not just the first, so
/// a real-e2e-test failure shows the whole picture at once.
pub fn validate_against(type_name: &str, instance: &Value) -> Vec<String> {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    register_all_defs(&mut generator);
    let defs = generator.take_definitions(true);
    if !defs.contains_key(type_name) {
        return vec![format!(
            "no such registered schema type: {type_name} (checked register_all_defs's $defs)"
        )];
    }
    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/$defs/{type_name}"),
        "$defs": Value::Object(defs),
    });
    format_errors(instance, type_name, &schema)
}

/// Looks up `method` in [`METHODS`] and validates `instance` against its
/// `params` schema. Panics if `method` isn't in the registry at all (a
/// caller passing a typo'd or genuinely-uncovered method name is a test
/// bug worth failing loudly on, not silently skipping) or has no params
/// schema at all (e.g. `agents/list`) -- callers of those methods should
/// not call this function for them.
pub fn validate_params(method: &str, instance: &Value) -> Vec<String> {
    let entry = METHODS
        .iter()
        .find(|m| m.method == method)
        .unwrap_or_else(|| panic!("no such method in acpx-proto::methods::METHODS: {method}"));
    let schema_ref = entry.params.unwrap_or_else(|| {
        panic!(
            "{method} has no params schema (see methods.rs) -- do not call validate_params for it"
        )
    });
    validate_against(schema_ref_name(schema_ref), instance)
}

/// Same as [`validate_params`] but for `result`. Panics the same way for
/// an unknown method or a method with no `result` schema (a true
/// notification, e.g. `session/cancel` -- there is no reply to validate).
pub fn validate_result(method: &str, instance: &Value) -> Vec<String> {
    let entry = METHODS
        .iter()
        .find(|m| m.method == method)
        .unwrap_or_else(|| panic!("no such method in acpx-proto::methods::METHODS: {method}"));
    let schema_ref = entry.result.unwrap_or_else(|| {
        panic!(
            "{method} has no result schema (see methods.rs) -- do not call validate_result for it"
        )
    });
    validate_against(schema_ref_name(schema_ref), instance)
}

fn schema_ref_name(schema_ref: SchemaRef) -> &'static str {
    match schema_ref {
        SchemaRef::Native(n) => n,
        SchemaRef::UpstreamAcp(n) => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_profile_schema_instance_passes() {
        let instance = serde_json::json!({
            "name": "work",
            "agent_id": "codex-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
            "permission_policy": "auto_reject",
            "allow_fs_access": false,
            "allow_terminal_access": false,
            "auth_method_id": null,
        });
        assert_eq!(
            validate_against("ProfileSchema", &instance),
            Vec::<String>::new()
        );
    }

    #[test]
    fn wrong_type_field_is_rejected() {
        let instance = serde_json::json!({
            "name": 123,
            "agent_id": "codex-acp",
        });
        let errors = validate_against("ProfileSchema", &instance);
        assert!(!errors.is_empty(), "expected a schema violation, got none");
    }

    #[test]
    fn unregistered_type_name_reports_a_clear_error_not_a_panic() {
        let errors = validate_against("TotallyMadeUpTypeName", &serde_json::json!({}));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("no such registered schema type"));
    }

    #[test]
    fn upstream_initialize_request_params_validate() {
        let instance = serde_json::json!({"protocolVersion": 1});
        let errors = validate_params("initialize", &instance);
        assert_eq!(errors, Vec::<String>::new(), "errors: {errors:?}");
    }

    #[test]
    fn session_prompt_params_matching_the_real_client_sdks_shape_validate() {
        let instance = serde_json::json!({
            "sessionId": "abc-123",
            "prompt": [{"type": "text", "text": "hello"}],
        });
        let errors = validate_params("session/prompt", &instance);
        assert_eq!(errors, Vec::<String>::new(), "errors: {errors:?}");
    }

    #[test]
    #[should_panic(expected = "no such method")]
    fn validate_params_panics_on_unknown_method() {
        validate_params("totally/bogus", &serde_json::json!({}));
    }
}
