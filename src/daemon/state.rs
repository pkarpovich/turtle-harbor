use crate::common::error::Result;
use crate::common::ipc::ProcessStatus;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScriptState {
    pub name: String,
    #[serde(default)]
    pub config_path: Option<PathBuf>,
    pub status: ProcessStatus,
    pub last_started: Option<DateTime<Local>>,
    pub last_stopped: Option<DateTime<Local>>,
    pub exit_code: Option<i32>,
    pub explicitly_stopped: bool,
    #[serde(default)]
    pub restart_count: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RunningState {
    pub version: u32,
    pub scripts: Vec<ScriptState>,
    pub state_file: PathBuf,
    #[serde(default)]
    pub config_path: Option<PathBuf>,
}

impl RunningState {
    pub fn new(state_file: PathBuf) -> Self {
        Self {
            version: 3,
            scripts: Vec::new(),
            state_file,
            config_path: None,
        }
    }

    pub fn load(state_file: &PathBuf) -> Result<Self> {
        if state_file.exists() {
            let content = std::fs::read_to_string(&state_file)?;
            let mut state: RunningState = serde_json::from_str(&content)?;
            state.state_file = state_file.clone();

            let legacy_config_path = state.config_path.take();
            for script in &mut state.scripts {
                if script.config_path.is_none() {
                    script.config_path = legacy_config_path.clone();
                }
            }

            Ok(state)
        } else {
            Ok(RunningState::new(state_file.clone()))
        }
    }

    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = self.state_file.with_extension("tmp");
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&tmp, &content).await?;
        fs::rename(&tmp, &self.state_file).await?;
        Ok(())
    }

    pub async fn update_script(&mut self, script: ScriptState) -> Result<()> {
        if let Some(existing) = self.scripts.iter_mut().find(|s| s.name == script.name) {
            *existing = script;
        } else {
            self.scripts.push(script);
        }
        self.save().await?;
        Ok(())
    }

    pub async fn remove_script(&mut self, name: &str) -> Result<()> {
        self.scripts.retain(|s| s.name != name);
        self.save().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn load_old_format_migrates_config_path() {
        let state_json = r#"{
            "version": 2,
            "state_file": "/tmp/state.json",
            "config_path": "/projects/a/scripts.yml",
            "scripts": [
                {
                    "name": "script1",
                    "status": "Running",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": false,
                    "restart_count": 0
                },
                {
                    "name": "script2",
                    "status": "Stopped",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": true,
                    "restart_count": 0
                }
            ]
        }"#;

        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), state_json).unwrap();
        let state = RunningState::load(&tmp.path().to_path_buf()).unwrap();

        assert_eq!(state.scripts.len(), 2);
        assert_eq!(
            state.scripts[0].config_path.as_deref(),
            Some(std::path::Path::new("/projects/a/scripts.yml"))
        );
        assert_eq!(
            state.scripts[1].config_path.as_deref(),
            Some(std::path::Path::new("/projects/a/scripts.yml"))
        );
        assert!(state.config_path.is_none());
    }

    #[test]
    fn load_new_format_preserves_per_script_config_path() {
        let state_json = r#"{
            "version": 3,
            "state_file": "/tmp/state.json",
            "scripts": [
                {
                    "name": "script1",
                    "config_path": "/projects/a/scripts.yml",
                    "status": "Running",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": false,
                    "restart_count": 0
                },
                {
                    "name": "script2",
                    "config_path": "/projects/b/scripts.yml",
                    "status": "Stopped",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": true,
                    "restart_count": 0
                }
            ]
        }"#;

        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), state_json).unwrap();
        let state = RunningState::load(&tmp.path().to_path_buf()).unwrap();

        assert_eq!(state.scripts.len(), 2);
        assert_eq!(
            state.scripts[0].config_path.as_deref(),
            Some(std::path::Path::new("/projects/a/scripts.yml"))
        );
        assert_eq!(
            state.scripts[1].config_path.as_deref(),
            Some(std::path::Path::new("/projects/b/scripts.yml"))
        );
    }

    #[test]
    fn load_mixed_format_migrates_only_missing() {
        let state_json = r#"{
            "version": 2,
            "state_file": "/tmp/state.json",
            "config_path": "/projects/legacy/scripts.yml",
            "scripts": [
                {
                    "name": "has_path",
                    "config_path": "/projects/explicit/scripts.yml",
                    "status": "Running",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": false,
                    "restart_count": 0
                },
                {
                    "name": "no_path",
                    "status": "Stopped",
                    "last_started": null,
                    "last_stopped": null,
                    "exit_code": null,
                    "explicitly_stopped": false,
                    "restart_count": 0
                }
            ]
        }"#;

        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), state_json).unwrap();
        let state = RunningState::load(&tmp.path().to_path_buf()).unwrap();

        assert_eq!(
            state.scripts[0].config_path.as_deref(),
            Some(std::path::Path::new("/projects/explicit/scripts.yml"))
        );
        assert_eq!(
            state.scripts[1].config_path.as_deref(),
            Some(std::path::Path::new("/projects/legacy/scripts.yml"))
        );
    }

    #[test]
    fn load_nonexistent_file_creates_empty_state() {
        let state = RunningState::load(&PathBuf::from("/nonexistent/state.json")).unwrap();
        assert_eq!(state.version, 3);
        assert!(state.scripts.is_empty());
        assert!(state.config_path.is_none());
    }
}
