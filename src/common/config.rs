use crate::common::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_MAX_RESTARTS: u32 = 5;
const DEFAULT_MAX_RESTARTS_CRON: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub settings: Settings,
    pub scripts: HashMap<String, Script>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LokiConfig {
    pub url: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub log_dir: PathBuf,
    #[serde(default)]
    pub loki: Option<LokiConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Script {
    pub command: String,
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub max_restarts: Option<u32>,
    pub cron: Option<String>,
}

impl Script {
    pub fn effective_max_restarts(&self) -> u32 {
        self.max_restarts.unwrap_or(if self.cron.is_some() {
            DEFAULT_MAX_RESTARTS_CRON
        } else {
            DEFAULT_MAX_RESTARTS
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RestartPolicy {
    Always,
    Never,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let config = serde_yml::from_str(&content)?;
        Ok(config)
    }
}
