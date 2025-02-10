use crate::common::config::RestartPolicy;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ScriptStatus {
    Running,
    Stopped,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScriptState {
    pub name: String,
    pub command: String,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub status: ScriptStatus,
    pub last_started: Option<DateTime<Local>>,
    pub last_stopped: Option<DateTime<Local>>,
    pub exit_code: Option<i32>,
    pub explicitly_stopped: bool,
    pub cron: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RunningState {
    pub version: u32,
    pub scripts: Vec<ScriptState>,
    pub state_file: PathBuf,
}

impl RunningState {
    pub fn new(state_file: PathBuf) -> Self {
        Self {
            version: 1,
            scripts: Vec::new(),
            state_file,
        }
    }

    pub fn load(state_file: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        if state_file.exists() {
            let content = std::fs::read_to_string(&state_file)?;
            let serializable: RunningState = serde_json::from_str(&content)?;
            Ok(Self {
                version: serializable.version,
                scripts: serializable.scripts.into_iter().map(Into::into).collect(),
                state_file: state_file.clone(),
            })
        } else {
            Ok(RunningState::new(state_file.clone()))
        }
    }

    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = self.state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.state_file, content)?;
        Ok(())
    }

    pub fn update_script(&mut self, script: ScriptState) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(existing) = self.scripts.iter_mut().find(|s| s.name == script.name) {
            *existing = script;
        } else {
            self.scripts.push(script);
        }
        self.save()?;
        Ok(())
    }

    pub fn remove_script(&mut self, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.scripts.retain(|s| s.name != name);
        self.save()?;
        Ok(())
    }
}
