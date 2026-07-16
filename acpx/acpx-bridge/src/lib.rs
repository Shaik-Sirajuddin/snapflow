//! Transport-independent policy for ACPX's strict-ACP `/acp` bridge.
//!
//! The bridge deliberately exposes models, never ACPX managed profiles.
//! `acpx-server` owns HTTP/WS framing while `acpx-core` remains the sole
//! owner of registry lookup, session routing, and backend supervision.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One public model alias mapped to an internal ACP adapter and its native
/// model identifier. The alias is intentionally namespaced (for example
/// `claude/sonnet`) so adapter-local names cannot collide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeModel {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub agent_id: String,
    pub model_id: String,
}

/// Bridge policy loaded only when `ACPX_ACP_BRIDGE_ENABLED=1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub default_model: String,
    pub models: Vec<BridgeModel>,
}

/// Public, secret-safe model entry returned by `/acp/models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicModel {
    pub id: String,
    pub name: String,
    pub agent_id: String,
    pub available: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BridgeConfigError {
    #[error("ACPX_ACP_BRIDGE_CONFIG_FILE must be set when ACPX_ACP_BRIDGE_ENABLED=1")]
    MissingConfigFile,
    #[error("failed to read ACP bridge config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse ACP bridge config {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("bridge default_model {0:?} is not declared in models")]
    UnknownDefaultModel(String),
    #[error("bridge model at index {index} has an empty {field}")]
    EmptyField { index: usize, field: &'static str },
    #[error("bridge model id {0:?} is declared more than once")]
    DuplicateModel(String),
    #[error("bridge model alias {0:?} is not declared")]
    UnknownModelAlias(String),
}

impl BridgeConfig {
    /// Returns `None` unless the operator explicitly enables the bridge.
    ///
    /// The default is deliberately off so existing ACPX deployments do not
    /// gain new public endpoints or a virtual-session behavior change.
    pub fn from_env() -> Result<Option<Self>, BridgeConfigError> {
        let enabled = std::env::var("ACPX_ACP_BRIDGE_ENABLED")
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"));
        if !enabled {
            return Ok(None);
        }
        let path = std::env::var("ACPX_ACP_BRIDGE_CONFIG_FILE")
            .map_err(|_| BridgeConfigError::MissingConfigFile)?;
        Self::from_file(Path::new(&path)).map(Some)
    }

    pub fn from_file(path: &Path) -> Result<Self, BridgeConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| BridgeConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Self =
            serde_json::from_str(&raw).map_err(|source| BridgeConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), BridgeConfigError> {
        let mut model_ids = HashSet::new();
        for (index, model) in self.models.iter().enumerate() {
            for (field, value) in [
                ("id", model.id.as_str()),
                ("agent_id", model.agent_id.as_str()),
                ("model_id", model.model_id.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(BridgeConfigError::EmptyField { index, field });
                }
            }
            if !model_ids.insert(model.id.as_str()) {
                return Err(BridgeConfigError::DuplicateModel(model.id.clone()));
            }
        }
        if !model_ids.contains(self.default_model.as_str()) {
            return Err(BridgeConfigError::UnknownDefaultModel(
                self.default_model.clone(),
            ));
        }
        Ok(())
    }

    /// Agent IDs ACPX must retain from its own `agents/list` response.
    pub fn agent_ids(&self) -> HashSet<&str> {
        self.models
            .iter()
            .map(|model| model.agent_id.as_str())
            .collect()
    }

    /// Returns the configured model for a public ACP alias.
    pub fn model_by_alias(&self, alias: &str) -> Option<&BridgeModel> {
        self.models.iter().find(|model| model.id == alias)
    }

    /// Resolves a public ACP alias, returning a clear error when it is not
    /// configured. Routers should use this before reading native adapter data.
    pub fn resolve_model(&self, alias: &str) -> Result<&BridgeModel, BridgeConfigError> {
        self.model_by_alias(alias)
            .ok_or_else(|| BridgeConfigError::UnknownModelAlias(alias.to_string()))
    }

    /// Builds the global ACP `configOptions` value for model selection.
    ///
    /// The result intentionally contains only public aliases and display
    /// names; native model and adapter identifiers remain bridge-internal.
    pub fn model_config_options(&self) -> serde_json::Value {
        serde_json::json!([{
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": self.default_model,
            "options": self.models.iter().map(|model| serde_json::json!({
                "value": model.id,
                "name": model.name.as_deref().unwrap_or(&model.id),
            })).collect::<Vec<_>>(),
        }])
    }

    /// Builds the secret-safe public model catalog from ACPX's native
    /// `agents/list` result. A model is available only when its adapter is
    /// currently detected as installed; model entitlement validation is a
    /// later probe phase and must not be guessed here.
    pub fn public_models(&self, agents_result: &serde_json::Value) -> Vec<PublicModel> {
        Self::public_models_for(&self.models, agents_result)
    }

    /// Same filtering as [`Self::public_models`], but accepts a runtime
    /// discovered catalog maintained by the bridge transport.
    pub fn public_models_for(
        models: &[BridgeModel],
        agents_result: &serde_json::Value,
    ) -> Vec<PublicModel> {
        let installed_agents: HashSet<&str> = agents_result
            .get("agents")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|agent| {
                (agent.get("status").and_then(serde_json::Value::as_str) == Some("installed"))
                    .then(|| agent.get("id").and_then(serde_json::Value::as_str))
                    .flatten()
            })
            .collect();

        models
            .iter()
            .map(|model| PublicModel {
                id: model.id.clone(),
                name: model.name.clone().unwrap_or_else(|| model.id.clone()),
                agent_id: model.agent_id.clone(),
                available: installed_agents.contains(model.agent_id.as_str()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BridgeConfig {
        BridgeConfig {
            default_model: "claude/sonnet".to_string(),
            models: vec![
                BridgeModel {
                    id: "claude/sonnet".to_string(),
                    name: Some("Claude Sonnet".to_string()),
                    agent_id: "claude-agent-acp".to_string(),
                    model_id: "sonnet".to_string(),
                },
                BridgeModel {
                    id: "codex/gpt-5.5".to_string(),
                    name: None,
                    agent_id: "codex-acp".to_string(),
                    model_id: "gpt-5.5".to_string(),
                },
            ],
        }
    }

    #[test]
    fn public_catalog_uses_native_agent_install_status_without_leaking_config() {
        let models = config().public_models(&serde_json::json!({
            "agents": [
                {"id": "claude-agent-acp", "status": "installed"},
                {"id": "codex-acp", "status": "not_installed"}
            ]
        }));
        assert_eq!(models[0].id, "claude/sonnet");
        assert!(models[0].available);
        assert_eq!(models[1].name, "codex/gpt-5.5");
        assert!(!models[1].available);
    }

    #[test]
    fn default_model_must_be_declared() {
        let mut config = config();
        config.default_model = "missing/model".to_string();
        assert!(matches!(
            config.validate(),
            Err(BridgeConfigError::UnknownDefaultModel(_))
        ));
    }

    #[test]
    fn duplicate_model_alias_is_rejected() {
        let mut config = config();
        config.models.push(config.models[0].clone());
        assert!(matches!(
            config.validate(),
            Err(BridgeConfigError::DuplicateModel(_))
        ));
    }

    #[test]
    fn model_alias_resolution_returns_only_declared_model() {
        let config = config();
        let model = config
            .model_by_alias("codex/gpt-5.5")
            .expect("configured alias");
        assert_eq!(model.agent_id, "codex-acp");
        assert_eq!(model.model_id, "gpt-5.5");
        assert!(config.model_by_alias("missing/model").is_none());
        assert!(matches!(
            config.resolve_model("missing/model"),
            Err(BridgeConfigError::UnknownModelAlias(alias)) if alias == "missing/model"
        ));
    }

    #[test]
    fn model_config_options_expose_only_public_aliases_and_names() {
        assert_eq!(
            config().model_config_options(),
            serde_json::json!([{
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "claude/sonnet",
                "options": [
                    {"value": "claude/sonnet", "name": "Claude Sonnet"},
                    {"value": "codex/gpt-5.5", "name": "codex/gpt-5.5"},
                ],
            }])
        );
    }
}
