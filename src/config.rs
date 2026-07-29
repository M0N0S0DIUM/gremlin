use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::ConfigError;

/// Top-level Gremlin configuration — lives at ~/.config/gremlin/config.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The small model that runs continuously for conversation/routing
    pub model: ModelConfig,

    /// Ollama connection
    pub ollama: OllamaConfig,

    /// Coding service (Hermes)
    #[serde(default)]
    pub hermes: Option<HermesConfig>,

    /// Vision model (loaded on demand for screenshots)
    #[serde(default)]
    pub vision: Option<VisionConfig>,

    /// Sprite/animation configuration
    #[serde(default)]
    pub sprite: Option<SpriteConfig>,

    /// Long-term preferences Gremlin remembers
    #[serde(default)]
    pub preferences: Preferences,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model name in Ollama (e.g. "llama3.2:3b", "qwen2.5:7b")
    pub name: String,

    /// System prompt that sets Gremlin's personality
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,

    /// Temperature for the small model
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// How long to keep model loaded in VRAM after last request.
    /// "0" = unload immediately, "5m" = 5 minutes, "-1" = never unload.
    /// Uses Ollama's keep_alive parameter.
    #[serde(default = "default_keep_alive")]
    pub keep_alive: String,
}

fn default_system_prompt() -> String {
    include_str!("../config/system_prompt.txt").to_string()
}

fn default_temperature() -> f32 {
    0.7
}

fn default_keep_alive() -> String {
    "5m".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaConfig {
    /// Base URL for Ollama API
    #[serde(default = "default_ollama_url")]
    pub url: String,

    /// Max context window for the small model
    #[serde(default = "default_context_size")]
    pub context_size: usize,

    /// Default keep_alive for models (passed to Ollama)
    #[serde(default = "default_keep_alive")]
    pub keep_alive: String,
}

fn default_ollama_url() -> String {
    // Use 127.0.0.1 explicitly, not "localhost" — on hosts with WSL installed,
    // "localhost" can resolve to ::1 which WSL's relay intercepts and forwards
    // into a separate (often model-less) Ollama instance inside the WSL VM,
    // causing intermittent "model not found" errors against the real
    // Windows-side Ollama on 127.0.0.1.
    "http://127.0.0.1:11434".to_string()
}

fn default_context_size() -> usize {
    8192
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HermesConfig {
    /// Path to Hermes CLI binary
    #[serde(default = "default_hermes_path")]
    pub binary: String,

    /// Default coding model to pass to Hermes
    pub coding_model: String,
}

fn default_hermes_path() -> String {
    "hermes".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionConfig {
    /// Vision model name in Ollama
    pub model: String,
}

/// Sprite/animation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpriteConfig {
    /// Path to sprite assets directory (relative to project root or absolute)
    /// Default: "assets/sprites"
    #[serde(default = "default_sprite_dir")]
    pub assets_dir: String,

    /// Initial animation state on startup
    #[serde(default = "default_sprite_state")]
    pub initial_state: String,
}

fn default_sprite_dir() -> String {
    "assets/sprites".to_string()
}

fn default_sprite_state() -> String {
    "idle".to_string()
}

impl Default for SpriteConfig {
    fn default() -> Self {
        Self {
            assets_dir: default_sprite_dir(),
            initial_state: default_sprite_state(),
        }
    }
}

/// Persistent preferences Gremlin learns over time
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preferences {
    pub preferred_coding_model: Option<String>,
    pub shell: Option<String>,
    pub terminal: Option<String>,
    pub desktop: Option<String>,
    pub theme: Option<String>,
    pub current_project: Option<String>,
}

impl Config {
    /// Load config from ~/.config/gremlin/config.toml, falling back to defaults
    pub fn load() -> Result<Self, ConfigError> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| ConfigError::MissingDir("XDG_CONFIG_HOME not set".into()))?
            .join("gremlin");

        let config_path = config_dir.join("config.toml");

        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            let config: Config = toml::from_str(&content)?;
            config.validate()?;
            Ok(config)
        } else {
            // Return defaults — caller can write them out
            let config = Config::default();
            config.validate()?;
            Ok(config)
        }
    }

    /// Validate that required fields are present
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.model.name.is_empty() {
            return Err(ConfigError::MissingField("model.name".into()));
        }
        if self.model.system_prompt.is_empty() {
            return Err(ConfigError::MissingField("model.system_prompt".into()));
        }
        Ok(())
    }

    /// Write config to disk, creating directories as needed
    pub fn save(&self) -> Result<(), std::io::Error> {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("gremlin");

        std::fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("config.toml");
        let content = toml::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
        })?;
        std::fs::write(&config_path, content)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: ModelConfig {
                name: "llama3.2:3b".to_string(),
                system_prompt: default_system_prompt(),
                temperature: default_temperature(),
                keep_alive: default_keep_alive(),
            },
            ollama: OllamaConfig {
                url: default_ollama_url(),
                context_size: default_context_size(),
                keep_alive: default_keep_alive(),
            },
            hermes: None,
            vision: None,
            sprite: Some(SpriteConfig::default()),
            preferences: Preferences::default(),
        }
    }
}