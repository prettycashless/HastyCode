//! Persistent local settings (`~/.hasty/config.json`). No secrets live here:
//! the config only stores references to keys held by the secret vault.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

use crate::paths;
use crate::permissions::Mode;

pub const CONFIG_VERSION: u32 = 1;

/// A provider account the user configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// One of: openai, anthropic, google, openrouter, groq, mistral, ollama,
    /// custom.
    pub id: String,
    /// Base URL override (required for `custom`, optional elsewhere).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the secret holding the API key (never the key itself).
    #[serde(default)]
    pub key_ref: Option<String>,
    /// Models discovered or entered manually.
    #[serde(default)]
    pub models: Vec<String>,
    /// True when Hasty detected a free-tier key for this provider.
    #[serde(default)]
    pub free_tier: bool,
}

/// An extra key used by subagents, with its own model and role label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentKey {
    pub name: String,
    pub role: String,
    pub provider: String,
    pub key_ref: String,
    pub model: String,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connector {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub enabled: bool,
    /// Secret reference for a token, when the connector needs one.
    #[serde(default)]
    pub token_ref: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    pub name: String,
    /// Color overrides as `#rrggbb`, keyed by theme slot.
    #[serde(default)]
    pub custom: BTreeMap<String, String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "dark-red".into(),
            custom: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub onboarded: bool,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub permission_mode: Mode,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default)]
    pub subagent_keys: Vec<AgentKey>,
    /// Repos installed under ~/.hasty/skills.
    #[serde(default)]
    pub skill_repos: Vec<String>,
    /// Skill names enabled for on-demand loading.
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub connectors: Vec<Connector>,
    #[serde(default)]
    pub max_iterations: u32,
    /// Where the vault keeps data: "keychain" or "file".
    #[serde(default)]
    pub secret_backend: String,
    #[serde(default)]
    pub allow_outside_project_reads: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            onboarded: false,
            provider: String::new(),
            model: String::new(),
            providers: Vec::new(),
            permission_mode: Mode::AskEveryTime,
            theme: ThemeConfig::default(),
            subagent_keys: Vec::new(),
            skill_repos: Vec::new(),
            enabled_skills: Vec::new(),
            connectors: Vec::new(),
            max_iterations: 24,
            secret_backend: String::new(),
            allow_outside_project_reads: false,
        }
    }
}

impl Config {
    pub fn load() -> io::Result<Config> {
        let path = paths::config_file();
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = paths::read_to_string(&path)?;
        match serde_json::from_str::<Config>(&raw) {
            Ok(cfg) => Ok(cfg),
            Err(_) => {
                // Keep the unreadable file rather than silently destroying it.
                let backup = path.with_extension(format!("corrupt-{}", now_secs()));
                let _ = std::fs::rename(&path, backup);
                Ok(Config::default())
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        paths::ensure_home()?;
        let raw = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        paths::write_private(&paths::config_file(), &raw)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn provider_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    /// Models available on the active provider.
    pub fn active_models(&self) -> Vec<String> {
        self.provider(&self.provider)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    }

    pub fn active_provider(&self) -> Option<&ProviderConfig> {
        self.provider(&self.provider)
    }

    pub fn key_ref_for(&self, provider: &str) -> Option<String> {
        self.provider(provider).and_then(|p| p.key_ref.clone())
    }

    pub fn theme_slots(&self) -> crate::theme::Theme {
        crate::theme::Theme::resolve(&self.theme)
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
