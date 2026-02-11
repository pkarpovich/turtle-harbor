use crate::common::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub settings: Settings,
    pub scripts: HashMap<String, Script>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub log_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Script {
    pub command: String,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub cron: Option<String>,
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
