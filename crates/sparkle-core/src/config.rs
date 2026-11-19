//! Configuration and model resolution.
//!
//! Settings from an existing Python `llm` install (keys, aliases, default model and
//! `extra-openai-models.yaml`) are read so that sparkle works immediately. That directory is
//! never written to.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::openai::OpenAiCompat;
use crate::{Error, Provider, Result};

const OPENAI_BASE: &str = "https://api.openai.com/v1";
const FALLBACK_MODEL: &str = "gpt-4o-mini";

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub llm_dir: PathBuf,
}

impl Paths {
    /// `SPARKLE_HOME` relocates all sparkle state and `LLM_USER_PATH` locates the Python llm settings.
    pub fn discover() -> Result<Self> {
        let dirs = directories::BaseDirs::new()
            .ok_or_else(|| Error::Config("could not determine the home directory".into()))?;
        let (config_dir, data_dir) = match std::env::var_os("SPARKLE_HOME") {
            Some(home) => (PathBuf::from(&home), PathBuf::from(home)),
            None => (dirs.config_dir().join("sparkle"), dirs.data_dir().join("sparkle")),
        };
        let llm_dir = std::env::var_os("LLM_USER_PATH")
            .map_or_else(|| dirs.config_dir().join("io.datasette.llm"), PathBuf::from);
        Ok(Self { config_dir, data_dir, llm_dir })
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("sparkle.db")
    }

    /// The local prompt pack made by `scripts/build-prompt-pack.py`.
    pub fn prompt_pack(&self) -> PathBuf {
        self.data_dir.join("prompt-pack")
    }

    /// The private superset of the pack, read only by builds with the `redteam` feature.
    pub fn redteam_pack(&self) -> PathBuf {
        self.data_dir.join("prompt-pack-redteam")
    }

    /// Holds the name of the default system prompt, set from `/prompts` or `sparkle prompts`.
    pub fn default_prompt(&self) -> PathBuf {
        self.config_dir.join("default_prompt.txt")
    }
}

/// `config.toml`. Every field is optional.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub default_model: Option<String>,
    pub effort: Option<String>,
    pub persona: Option<String>,
    pub theme: Option<String>,
    pub icons: Option<String>,
    pub animations: Option<bool>,
    /// Show the model's reasoning as it arrives. Defaults to true.
    pub show_thinking: Option<bool>,
    /// Ask the model to name each new chat. Defaults to true.
    pub smart_titles: Option<bool>,
    /// Warn once a request fills this percentage of the context window. Defaults to 80.
    pub context_warn_percent: Option<u8>,
    pub models: Vec<ModelConfig>,
}

impl Settings {
    /// The warning threshold, kept to a percentage that can actually be reached.
    pub fn warn_percent(&self) -> u8 {
        self.context_warn_percent.unwrap_or(crate::context::DEFAULT_WARN_PERCENT).clamp(1, 100)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub id: String,
    pub name: Option<String>,
    pub api_base: String,
    pub key_name: Option<String>,
    pub key_env: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LlmExtraModel {
    model_id: String,
    model_name: Option<String>,
    api_base: Option<String>,
    api_key_name: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub id: String,
    pub wire_name: String,
    pub api_base: String,
    pub key_name: Option<String>,
    pub key_env: Option<String>,
    pub aliases: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Registry {
    pub settings: Settings,
    pub models: Vec<ModelEntry>,
    aliases: BTreeMap<String, String>,
    keys: BTreeMap<String, String>,
    llm_default: Option<String>,
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Config(format!("could not read {}: {error}", path.display()))
}

impl Registry {
    pub fn load(paths: &Paths) -> Result<Self> {
        let mut registry = Registry::default();

        let path = paths.config_dir.join("config.toml");
        if let Some(text) = read_optional(&path)? {
            registry.settings = toml::from_str(&text).map_err(|error| invalid(&path, error))?;
        }
        for model in registry.settings.models.clone() {
            registry.models.push(ModelEntry {
                wire_name: model.name.unwrap_or_else(|| model.id.clone()),
                id: model.id,
                api_base: model.api_base,
                key_name: model.key_name,
                key_env: model.key_env,
                aliases: model.aliases,
            });
        }

        let path = paths.llm_dir.join("extra-openai-models.yaml");
        if let Some(text) = read_optional(&path)? {
            let extras: Option<Vec<LlmExtraModel>> =
                serde_norway::from_str(&text).map_err(|error| invalid(&path, error))?;
            for model in extras.unwrap_or_default() {
                if registry.models.iter().any(|known| known.id == model.model_id) {
                    continue;
                }
                registry.models.push(ModelEntry {
                    wire_name: model.model_name.unwrap_or_else(|| model.model_id.clone()),
                    id: model.model_id,
                    api_base: model.api_base.unwrap_or_else(|| OPENAI_BASE.to_string()),
                    key_name: model.api_key_name,
                    key_env: None,
                    aliases: model.aliases,
                });
            }
        }

        let path = paths.llm_dir.join("aliases.json");
        if let Some(text) = read_optional(&path)? {
            registry.aliases = serde_json::from_str(&text).map_err(|error| invalid(&path, error))?;
        }

        let path = paths.llm_dir.join("keys.json");
        if let Some(text) = read_optional(&path)? {
            let values: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(&text).map_err(|error| invalid(&path, error))?;
            registry.keys = values
                .into_iter()
                .filter_map(|(name, value)| Some((name, value.as_str()?.to_string())))
                .collect();
        }

        let path = paths.llm_dir.join("default_model.txt");
        registry.llm_default =
            read_optional(&path)?.map(|text| text.trim().to_string()).filter(|s| !s.is_empty());
        Ok(registry)
    }

    pub fn default_model(&self) -> &str {
        self.settings
            .default_model
            .as_deref()
            .or(self.llm_default.as_deref())
            .unwrap_or(FALLBACK_MODEL)
    }

    /// Finds a model by id or alias. Unlisted OpenAI-style names use the OpenAI endpoint.
    pub fn resolve(&self, name: &str) -> Result<ModelEntry> {
        let target = self.aliases.get(name).map_or(name, String::as_str);
        let known = self.models.iter().find(|model| {
            model.id == target || model.aliases.iter().any(|alias| alias == target)
        });
        if let Some(model) = known {
            return Ok(model.clone());
        }
        let openai_style = target.starts_with("gpt-")
            || target.strip_prefix('o').is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        if openai_style {
            return Ok(ModelEntry {
                id: target.to_string(),
                wire_name: target.to_string(),
                api_base: OPENAI_BASE.to_string(),
                key_name: Some("openai".into()),
                key_env: Some("OPENAI_API_KEY".into()),
                aliases: Vec::new(),
            });
        }
        Err(Error::Config(format!(
            "unknown model '{name}'. Run `sparkle models` to see what is available"
        )))
    }

    /// Key order: `--key` (a stored key name or a literal), environment variable, stored key.
    pub fn api_key(&self, model: &ModelEntry, explicit: Option<&str>) -> Option<String> {
        if let Some(value) = explicit {
            return Some(self.keys.get(value).cloned().unwrap_or_else(|| value.to_string()));
        }
        let from_env = model
            .key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty());
        from_env.or_else(|| self.keys.get(model.key_name.as_deref()?).cloned())
    }

    pub fn provider(&self, model: &ModelEntry, explicit_key: Option<&str>) -> Arc<dyn Provider> {
        Arc::new(OpenAiCompat::new(model.api_base.clone(), self.api_key(model, explicit_key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut registry = Registry::default();
        registry.models.push(ModelEntry {
            id: "bonsai".into(),
            wire_name: "bonsai".into(),
            api_base: "http://localhost:8080/v1".into(),
            key_name: Some("home".into()),
            key_env: None,
            aliases: vec!["b".into()],
        });
        registry.aliases.insert("fast".into(), "bonsai".into());
        registry.keys.insert("home".into(), "stored".into());
        registry
    }

    #[test]
    fn resolves_ids_and_both_alias_sources() {
        let registry = registry();
        for name in ["bonsai", "b", "fast"] {
            assert_eq!(registry.resolve(name).unwrap().id, "bonsai");
        }
    }

    #[test]
    fn unknown_models_are_rejected_but_openai_names_pass() {
        let registry = registry();
        assert!(registry.resolve("nope").is_err());
        assert_eq!(registry.resolve("gpt-5").unwrap().api_base, OPENAI_BASE);
        assert_eq!(registry.resolve("o3").unwrap().api_base, OPENAI_BASE);
    }

    #[test]
    fn explicit_key_can_name_a_stored_key() {
        let registry = registry();
        let model = registry.resolve("bonsai").unwrap();
        assert_eq!(registry.api_key(&model, None).as_deref(), Some("stored"));
        assert_eq!(registry.api_key(&model, Some("home")).as_deref(), Some("stored"));
        assert_eq!(registry.api_key(&model, Some("literal")).as_deref(), Some("literal"));
    }
}
