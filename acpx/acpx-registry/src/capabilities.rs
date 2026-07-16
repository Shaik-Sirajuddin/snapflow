//! Normalized adapter capabilities learned from ACP handshakes.
//!
//! The official ACP registry identifies adapters and their launch
//! distributions. Models and permission modes are adapter/runtime data, so
//! they are discovered from `initialize` plus a disposable `session/new`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectOption {
    pub value: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub current_value: Option<String>,
    pub options: Vec<SelectOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    pub adapter_id: String,
    pub adapter_version: Option<String>,
    pub models: Vec<SelectOption>,
    pub permission_modes: Vec<SelectOption>,
    pub config_options: Vec<ConfigOption>,
    pub auth_methods: Vec<String>,
}

impl AdapterCapabilities {
    /// Normalizes the public ACP shapes without retaining opaque metadata,
    /// provider keys, or backend-specific command configuration.
    pub fn from_acp(
        adapter_id: impl Into<String>,
        initialize_result: &Value,
        new_session_result: &Value,
    ) -> Self {
        let config_options = new_session_result
            .get("configOptions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_config_option)
            .collect::<Vec<_>>();

        let models = config_options
            .iter()
            .filter(|option| option.category.as_deref() == Some("model") || option.id == "model")
            .flat_map(|option| option.options.clone())
            .collect();
        let permission_modes = config_options
            .iter()
            .filter(|option| {
                matches!(
                    option.category.as_deref(),
                    Some("permission") | Some("mode")
                ) || matches!(option.id.as_str(), "permissionMode" | "mode")
            })
            .flat_map(|option| option.options.clone())
            .collect();

        Self {
            adapter_id: adapter_id.into(),
            adapter_version: initialize_result
                .pointer("/agentInfo/version")
                .or_else(|| initialize_result.pointer("/agentVersion"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            models,
            permission_modes,
            config_options,
            auth_methods: initialize_result
                .get("authMethods")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|method| method.get("id").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect(),
        }
    }
}

fn parse_config_option(value: &Value) -> Option<ConfigOption> {
    let id = value.get("id")?.as_str()?.to_owned();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let options = value
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some(SelectOption {
                value: option.get("value")?.as_str()?.to_owned(),
                name: option
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| option.get("label").and_then(Value::as_str))
                    .unwrap_or(option.get("value")?.as_str()?)
                    .to_owned(),
            })
        })
        .collect();
    Some(ConfigOption {
        id,
        name,
        category: value
            .get("category")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        current_value: value
            .get("currentValue")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_models_permission_modes_and_auth_without_metadata() {
        let capabilities = AdapterCapabilities::from_acp(
            "claude-acp",
            &json!({
                "agentInfo": {"version": "0.59.0"},
                "authMethods": [{"id": "oauth"}, {"id": "api-key", "secret": "ignored"}]
            }),
            &json!({
                "configOptions": [
                    {
                        "id": "model", "name": "Model", "category": "model",
                        "currentValue": "haiku",
                        "options": [{"value": "haiku", "name": "Claude Haiku"}]
                    },
                    {
                        "id": "permissionMode", "name": "Permissions", "category": "permission",
                        "options": [{"value": "acceptEdits", "label": "Accept edits"}]
                    }
                ],
                "_meta": {"token": "must not be retained"}
            }),
        );

        assert_eq!(capabilities.adapter_version.as_deref(), Some("0.59.0"));
        assert_eq!(capabilities.models[0].value, "haiku");
        assert_eq!(capabilities.permission_modes[0].value, "acceptEdits");
        assert_eq!(capabilities.auth_methods, ["oauth", "api-key"]);
        assert_eq!(capabilities.config_options.len(), 2);
    }
}
