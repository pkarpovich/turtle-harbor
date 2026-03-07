use crate::common::config::{Config, LokiConfig, Script};
use crate::common::error::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct ConfigDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

pub struct ConfigManager {
    configs: HashMap<PathBuf, Config>,
}

impl ConfigManager {
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
        }
    }

    pub fn load(&mut self, path: &Path) -> Result<()> {
        let config = Config::load(path)?;
        self.configs.insert(path.to_path_buf(), config);
        Ok(())
    }

    pub fn config(&self, config_path: &Path) -> Result<&Config> {
        self.configs.get(config_path).ok_or(Error::ConfigNotLoaded)
    }

    pub fn config_dir(&self, config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_default()
    }

    pub fn script(&self, config_path: &Path, name: &str) -> Option<&Script> {
        self.configs
            .get(config_path)
            .and_then(|c| c.scripts.get(name))
    }

    pub fn script_names(&self, config_path: &Path) -> Vec<String> {
        self.configs
            .get(config_path)
            .map(|c| c.scripts.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_script(&self, config_path: &Path, name: &str) -> bool {
        self.configs
            .get(config_path)
            .map_or(false, |c| c.scripts.contains_key(name))
    }

    pub fn has_script_globally(&self, name: &str) -> Option<PathBuf> {
        self.configs.iter().find_map(|(path, config)| {
            if config.scripts.contains_key(name) {
                Some(path.clone())
            } else {
                None
            }
        })
    }

    pub fn log_dir(&self, config_path: &Path) -> Option<&Path> {
        self.configs
            .get(config_path)
            .map(|c| c.settings.log_dir.as_path())
    }

    pub fn loki_config(&self, config_path: &Path) -> Option<&LokiConfig> {
        self.configs
            .get(config_path)
            .and_then(|c| c.settings.loki.as_ref())
    }

    pub fn reload(&mut self, config_path: &Path) -> Result<ConfigDiff> {
        tracing::info!(config = ?config_path, "Reloading configuration");

        let old_config = self
            .configs
            .get(config_path)
            .ok_or(Error::ConfigNotLoaded)?
            .clone();

        let new_config = Config::load(config_path)?;
        self.configs.insert(config_path.to_path_buf(), new_config);

        let new_config = self.configs.get(config_path).expect("just inserted");

        let old_names: HashSet<String> = old_config.scripts.keys().cloned().collect();
        let new_names: HashSet<String> = new_config.scripts.keys().cloned().collect();

        let removed = old_names.difference(&new_names).cloned().collect();
        let added = new_names.difference(&old_names).cloned().collect();

        let changed = old_names
            .intersection(&new_names)
            .filter(|name| old_config.scripts[*name] != new_config.scripts[*name])
            .cloned()
            .collect();

        Ok(ConfigDiff {
            added,
            removed,
            changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    const CONFIG_A: &str = r#"
settings:
  log_dir: "./logs"
scripts:
  script_a1:
    command: "echo a1"
    restart_policy: "never"
  script_a2:
    command: "echo a2"
    restart_policy: "always"
"#;

    const CONFIG_B: &str = r#"
settings:
  log_dir: "./logs-b"
scripts:
  script_b1:
    command: "echo b1"
    restart_policy: "never"
"#;

    #[test]
    fn load_multiple_configs() {
        let file_a = write_config(CONFIG_A);
        let file_b = write_config(CONFIG_B);
        let mut mgr = ConfigManager::new();

        mgr.load(file_a.path()).unwrap();
        mgr.load(file_b.path()).unwrap();

        assert!(mgr.has_script(file_a.path(), "script_a1"));
        assert!(mgr.has_script(file_a.path(), "script_a2"));
        assert!(!mgr.has_script(file_a.path(), "script_b1"));
        assert!(mgr.has_script(file_b.path(), "script_b1"));
        assert!(!mgr.has_script(file_b.path(), "script_a1"));
    }

    #[test]
    fn script_lookup_across_configs() {
        let file_a = write_config(CONFIG_A);
        let file_b = write_config(CONFIG_B);
        let mut mgr = ConfigManager::new();

        mgr.load(file_a.path()).unwrap();
        mgr.load(file_b.path()).unwrap();

        let script = mgr.script(file_a.path(), "script_a1").unwrap();
        assert_eq!(script.command, "echo a1");

        let script = mgr.script(file_b.path(), "script_b1").unwrap();
        assert_eq!(script.command, "echo b1");

        assert!(mgr.script(file_a.path(), "script_b1").is_none());
    }

    #[test]
    fn has_script_globally_finds_across_configs() {
        let file_a = write_config(CONFIG_A);
        let file_b = write_config(CONFIG_B);
        let mut mgr = ConfigManager::new();

        mgr.load(file_a.path()).unwrap();
        mgr.load(file_b.path()).unwrap();

        assert_eq!(
            mgr.has_script_globally("script_a1").as_deref(),
            Some(file_a.path())
        );
        assert_eq!(
            mgr.has_script_globally("script_b1").as_deref(),
            Some(file_b.path())
        );
        assert!(mgr.has_script_globally("nonexistent").is_none());
    }

    #[test]
    fn script_names_per_config() {
        let file_a = write_config(CONFIG_A);
        let file_b = write_config(CONFIG_B);
        let mut mgr = ConfigManager::new();

        mgr.load(file_a.path()).unwrap();
        mgr.load(file_b.path()).unwrap();

        let mut names_a = mgr.script_names(file_a.path());
        names_a.sort();
        assert_eq!(names_a, vec!["script_a1", "script_a2"]);

        let names_b = mgr.script_names(file_b.path());
        assert_eq!(names_b, vec!["script_b1"]);

        assert!(mgr.script_names(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn reload_detects_changes() {
        let config_v1 = r#"
settings:
  log_dir: "./logs"
scripts:
  kept:
    command: "echo kept"
    restart_policy: "never"
  removed:
    command: "echo removed"
    restart_policy: "never"
  changed:
    command: "echo old"
    restart_policy: "never"
"#;
        let config_v2 = r#"
settings:
  log_dir: "./logs"
scripts:
  kept:
    command: "echo kept"
    restart_policy: "never"
  added:
    command: "echo added"
    restart_policy: "never"
  changed:
    command: "echo new"
    restart_policy: "never"
"#;
        let file = write_config(config_v1);
        let mut mgr = ConfigManager::new();
        mgr.load(file.path()).unwrap();

        std::fs::write(file.path(), config_v2).unwrap();
        let diff = mgr.reload(file.path()).unwrap();

        assert_eq!(diff.added, vec!["added"]);
        assert_eq!(diff.removed, vec!["removed"]);
        assert_eq!(diff.changed, vec!["changed"]);
    }

    #[test]
    fn config_dir_returns_parent() {
        let mgr = ConfigManager::new();
        assert_eq!(
            mgr.config_dir(Path::new("/projects/foo/scripts.yml")),
            PathBuf::from("/projects/foo")
        );
    }
}
