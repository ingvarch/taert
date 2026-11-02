//! Stores for ignore lists and configuration.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IgnoreStore {
    #[serde(default)]
    pub roots: Vec<String>,
}

impl IgnoreStore {
    pub fn new(roots: Vec<String>) -> Self {
        Self { roots }
    }

    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("taert").join("ignore.toml"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read ignore list from {}", path.display()))?;
        let store: IgnoreStore = toml::from_str(&content)
            .with_context(|| format!("Failed to parse ignore list from {}", path.display()))?;
        Ok(store)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, content)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    pub fn add(&mut self, path: String) -> bool {
        let normalized = if path.ends_with('/') && path.len() > 1 {
            path.trim_end_matches('/').to_string()
        } else {
            path
        };

        if !self.roots.contains(&normalized) {
            self.roots.push(normalized);
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, path: &str) -> bool {
        let trimmed = path.trim_end_matches('/');
        let initial_len = self.roots.len();
        self.roots
            .retain(|r| r.as_str() != path && r.as_str() != trimmed);
        self.roots.len() < initial_len
    }

    pub fn contains(&self, path: &str) -> bool {
        let trimmed = path.trim_end_matches('/');
        self.roots
            .iter()
            .any(|r| r.as_str() == path || r.as_str() == trimmed)
    }
}

use super::schedule::ScheduleFrequency;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSkip {
    pub at: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    #[serde(default = "default_schedule_every")]
    pub every: ScheduleFrequency,
    #[serde(default = "default_schedule_categories")]
    pub categories: Vec<String>,
    #[serde(default = "default_schedule_min_reclaim")]
    pub min_reclaim: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_skip: Option<ScheduleSkip>,
}

fn default_schedule_every() -> ScheduleFrequency {
    ScheduleFrequency::Week
}

fn default_schedule_categories() -> Vec<String> {
    vec!["dev".to_string(), "junk".to_string()]
}

fn default_schedule_min_reclaim() -> u64 {
    1_000_000_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_older_than_days")]
    pub older_than_days: u64,
    #[serde(default = "default_appearance")]
    pub appearance: String,
    #[serde(default)]
    pub extra_artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleConfig>,
}

fn default_older_than_days() -> u64 {
    7
}

fn default_appearance() -> String {
    "auto".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            older_than_days: default_older_than_days(),
            appearance: default_appearance(),
            extra_artifacts: Vec::new(),
            schedule: None,
        }
    }
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        Some(Self::path_in(&dirs::config_dir()?.join("taert")))
    }

    /// The config file inside `dir`. An old `config.json` stays in use until a TOML file exists.
    pub fn path_in(dir: &Path) -> PathBuf {
        let toml_path = dir.join("config.toml");
        let json_path = dir.join("config.json");
        if json_path.exists() && !toml_path.exists() {
            json_path
        } else {
            toml_path
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config from {}", path.display()))?;
        if path.extension().is_some_and(|ext| ext == "json") {
            let config: Config = serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse config from {}", path.display()))?;
            Ok(config)
        } else {
            let config: Config = toml::from_str(&content)
                .with_context(|| format!("Failed to parse config from {}", path.display()))?;
            Ok(config)
        }
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = if path.extension().is_some_and(|ext| ext == "json") {
            serde_json::to_string_pretty(self)?
        } else {
            toml::to_string_pretty(self)?
        };
        let temp_path = path.with_extension("tmp");
        fs::write(&temp_path, content)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }
}
