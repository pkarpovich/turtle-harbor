use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub settings: Settings,
    pub scripts: HashMap<String, Script>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub log_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Script {
    pub command: String,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestartPolicy {
    Always,
    Never,
}

impl Config {
    pub fn load(path: &str) -> crate::common::error::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config = serde_yaml::from_str(&content)
            .map_err(|e| crate::common::error::Error::Config(e.to_string()))?;
        Ok(config)
    }
}
