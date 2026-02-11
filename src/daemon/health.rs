use chrono::{DateTime, Local};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptHealthState {
    Running,
    Succeeded,
    Failed,
    NeverRan,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptHealth {
    pub name: String,
    pub healthy: bool,
    pub state: ScriptHealthState,
    pub last_exit_code: Option<i32>,
    pub last_run_at: Option<DateTime<Local>>,
    pub last_finished_at: Option<DateTime<Local>>,
    pub pid: Option<u32>,
    pub restart_count: u32,
}

impl ScriptHealth {
    pub fn never_ran(name: String) -> Self {
        Self {
            name,
            healthy: true,
            state: ScriptHealthState::NeverRan,
            last_exit_code: None,
            last_run_at: None,
            last_finished_at: None,
            pid: None,
            restart_count: 0,
        }
    }
}

pub type HealthSnapshot = Arc<RwLock<HashMap<String, ScriptHealth>>>;

pub fn new_health_snapshot() -> HealthSnapshot {
    Arc::new(RwLock::new(HashMap::new()))
}
