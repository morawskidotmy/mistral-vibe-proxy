//! Vibe's model registry: the built-in Mistral model plus any `[[models]]`
//! entries from `$VIBE_HOME/config.toml`.
//!
//! Vibe's `ModelConfig` has a `name` (the id sent to the provider API) and an
//! `alias` (what the user selects). The proxy accepts either in a request's
//! `model` field and maps the alias onto the name, so OpenAI clients can use
//! Vibe's alias vocabulary.
//!
//! Model IDs served to Mistral Vibe subscriptions. Per /v1/models these are aliases:
//! - mistral-vibe-cli-latest -> Mistral Medium
//! - mistral-vibe-cli-with-tools -> Mistral Medium
//! - mistral-vibe-cli-fast -> Mistral Small

use std::collections::HashMap;

use crate::key::vibe_home;

/// The model id vibe's default config sends to api.mistral.ai.
pub const DEFAULT_MODEL_NAME: &str = "mistral-vibe-cli-latest";

/// Vibe model specifications with their actual Mistral model mappings
pub const VIBE_MODELS: &[(&str, &str)] = &[
    ("mistral-vibe-cli-latest", "mistral-medium-latest"),
    ("mistral-vibe-cli-with-tools", "mistral-medium-latest"),
    ("mistral-vibe-cli-fast", "mistral-small-latest"),
];

#[derive(Clone)]
pub struct ModelRegistry {
    /// alias -> provider model name (includes name -> name identity entries).
    aliases: HashMap<String, String>,
    /// All known ids, advertised on /v1/models.
    pub ids: Vec<String>,
}

impl ModelRegistry {
    pub fn load() -> Self {
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut ids: Vec<String> = Vec::new();

        // Add built-in Vibe model mappings
        for (alias, name) in VIBE_MODELS {
            aliases.insert(alias.to_string(), name.to_string());
            aliases.insert(name.to_string(), name.to_string());
            if !ids.iter().any(|x| x == alias) {
                ids.push(alias.to_string());
            }
            if !ids.iter().any(|x| x == name) {
                ids.push(name.to_string());
            }
        }

        // Add default model if not already present
        if !ids.iter().any(|x| x == DEFAULT_MODEL_NAME) {
            ids.push(DEFAULT_MODEL_NAME.to_string());
        }

        let config_path = vibe_home().join("config.toml");
        let Ok(raw) = std::fs::read_to_string(&config_path) else {
            return Self { aliases, ids };
        };
        let Ok(cfg) = raw.parse::<toml::Table>() else {
            tracing::warn!("could not parse {}; model aliases ignored", config_path.display());
            return Self { aliases, ids };
        };
        let Some(models) = cfg.get("models").and_then(|m| m.as_array()) else {
            return Self { aliases, ids };
        };
        for m in models {
            let Some(table) = m.as_table() else { continue };
            let Some(name) = table.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let alias = table
                .get("alias")
                .and_then(|v| v.as_str())
                .unwrap_or(name);
            aliases.insert(alias.to_string(), name.to_string());
            aliases.insert(name.to_string(), name.to_string());
            for id in [&alias.to_string(), &name.to_string()] {
                if !ids.iter().any(|x| x == id) {
                    ids.push(id.clone());
                }
            }
        }
        Self { aliases, ids }
    }

    /// Map a requested model onto the provider model name. Unknown ids pass
    /// through unchanged so any real Mistral model still works.
    pub fn resolve(&self, model: &str) -> String {
        self.aliases
            .get(model)
            .cloned()
            .unwrap_or_else(|| model.to_string())
    }
}
