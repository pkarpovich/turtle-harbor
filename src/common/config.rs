use crate::common::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ffi::OsString;

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
    #[serde(default)]
    pub context: Option<PathBuf>,
    #[serde(default)]
    pub venv: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

impl Script {
    pub fn effective_max_restarts(&self) -> u32 {
        self.max_restarts.unwrap_or(if self.cron.is_some() {
            DEFAULT_MAX_RESTARTS_CRON
        } else {
            DEFAULT_MAX_RESTARTS
        })
    }

    pub fn resolved_context(&self) -> Option<PathBuf> {
        self.context.as_ref().map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            }
        })
    }

    pub fn resolved_env(&self) -> HashMap<OsString, OsString> {
        let mut env_vars: HashMap<OsString, OsString> = HashMap::new();
        let base_dir = self.resolved_context().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        if let Some(venv) = &self.venv {
            let venv_path = if venv.is_absolute() {
                venv.clone()
            } else {
                base_dir.join(venv)
            };
            let venv_bin = venv_path.join("bin");

            let current_path = std::env::var_os("PATH").unwrap_or_default();
            let mut new_path = OsString::from(&venv_bin);
            new_path.push(":");
            new_path.push(&current_path);

            env_vars.insert("PATH".into(), new_path);
            env_vars.insert("VIRTUAL_ENV".into(), venv_path.into_os_string());
        }

        if let Some(user_env) = &self.env {
            for (k, v) in user_env {
                env_vars.insert(k.into(), v.into());
            }
        }

        env_vars
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
