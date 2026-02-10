use crate::common::config::{Config, Script};
use crate::common::error::{Error, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct ConfigDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

pub struct ConfigManager {
    config: Option<Config>,
    config_path: Option<PathBuf>,
}

impl ConfigManager {
    pub fn new() -> Self {
        Self {
            config: None,
            config_path: None,
        }
    }

    pub fn load(&mut self, path: &Path) -> Result<()> {
        let config = Config::load(path)?;
        self.config = Some(config);
        self.config_path = Some(path.to_path_buf());
        Ok(())
    }

    pub fn config(&self) -> Result<&Config> {
        self.config.as_ref().ok_or(Error::ConfigNotLoaded)
    }

    pub fn script(&self, name: &str) -> Option<&Script> {
        self.config.as_ref().and_then(|c| c.scripts.get(name))
    }

    pub fn script_names(&self) -> Vec<String> {
        self.config
            .as_ref()
            .map(|c| c.scripts.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_script(&self, name: &str) -> bool {
        self.config
            .as_ref()
            .map_or(false, |c| c.scripts.contains_key(name))
    }

    pub fn log_dir(&self) -> Option<&Path> {
        self.config.as_ref().map(|c| c.settings.log_dir.as_path())
    }

    pub fn reload(&mut self) -> Result<ConfigDiff> {
        let config_path = self
            .config_path
            .clone()
            .ok_or(Error::ConfigNotLoaded)?;

        tracing::info!(config = ?config_path, "Reloading configuration");
        let new_config = Config::load(&config_path)?;
        let old_config = self.config.replace(new_config);

        let Some(old_config) = old_config else {
            return Ok(ConfigDiff {
                added: self.script_names(),
                removed: Vec::new(),
                changed: Vec::new(),
            });
        };

        let old_names: HashSet<String> = old_config.scripts.keys().cloned().collect();
        let new_names: HashSet<String> = self.script_names().into_iter().collect();

        let removed = old_names.difference(&new_names).cloned().collect();
        let added = new_names.difference(&old_names).cloned().collect();

        let new_scripts = &self.config.as_ref().unwrap().scripts;
        let changed = old_names
            .intersection(&new_names)
            .filter(|name| old_config.scripts[*name] != new_scripts[*name])
            .cloned()
            .collect();

        Ok(ConfigDiff {
            added,
            removed,
            changed,
        })
    }
}
