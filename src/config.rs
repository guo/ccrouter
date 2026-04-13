use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiFormat {
    Anthropic,
    OpenAI,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ModelMap {
    #[serde(flatten)]
    pub mappings: HashMap<String, String>,
    pub default_model: Option<String>,
}

impl ModelMap {
    pub fn resolve(&self, model: &str) -> String {
        if let Some(mapped) = self.mappings.get(model) {
            return mapped.clone();
        }
        if let Some(default) = &self.default_model {
            return default.clone();
        }
        model.to_string()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: String,
    pub format: ApiFormat,
    #[serde(default)]
    pub model_map: ModelMap,
}

impl Profile {
    pub fn api_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() {
            return None;
        }
        std::env::var(&self.api_key_env).ok()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxySettings {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            log_level: default_log_level(),
        }
    }
}

fn default_port() -> u16 {
    15721
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActiveConfig {
    pub profile: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxySettings,
    pub active: ActiveConfig,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl Config {
    pub fn active_profile(&self) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == self.active.profile)
    }
}

pub fn find_config_path() -> PathBuf {
    // 1. Local file takes priority
    let local = PathBuf::from("ccrouter.toml");
    if local.exists() {
        return local;
    }
    // 2. XDG config dir
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{}/.config/ccrouter/config.toml", home))
}

pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read config file: {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Invalid TOML in config file: {}", path.display()))?;
    Ok(config)
}

/// Rewrite only the `[active] profile = "..."` line in the config file.
pub fn write_active_profile(path: &Path, profile_id: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read config file: {}", path.display()))?;

    let updated = content
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("profile") && line.contains('=') {
                format!("profile = {:?}", profile_id)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Atomic write
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated)?;
    std::fs::rename(&tmp, path)?;
    info!("Switched active profile to '{}'", profile_id);
    Ok(())
}

/// Spawn a background task that watches the config file for changes
/// and sends the reloaded Config over the channel.
pub fn watch_config(path: PathBuf, tx: mpsc::Sender<Config>) -> Result<()> {
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();

        let mut watcher = match RecommendedWatcher::new(inner_tx, notify::Config::default()) {
            Ok(w) => w,
            Err(e) => {
                warn!("Cannot create file watcher: {}", e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
            warn!("Cannot watch config file {}: {}", path.display(), e);
            return;
        }

        info!("Watching config file for changes: {}", path.display());

        for res in inner_rx {
            match res {
                Ok(event) => {
                    use notify::EventKind::*;
                    match event.kind {
                        Modify(_) | Create(_) => {
                            // Small delay to let the write complete
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            match load_config(&path) {
                                Ok(cfg) => {
                                    info!(
                                        "Config reloaded — active profile: '{}'",
                                        cfg.active.profile
                                    );
                                    let _ = tx.blocking_send(cfg);
                                }
                                Err(e) => warn!("Config reload failed: {}", e),
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => warn!("File watch error: {}", e),
            }
        }
    });

    Ok(())
}
