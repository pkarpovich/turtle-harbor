use crate::common::ipc::ProcessStatus;
use crate::daemon::log_monitor::ScriptLogger;
use chrono::{DateTime, Local};
use tokio::task::JoinHandle;

pub struct ManagedProcess {
    pub pid: u32,
    pub status: ProcessStatus,
    pub start_time: Option<DateTime<Local>>,
    pub restart_count: u32,
    pub logger: Option<ScriptLogger>,
    pub watcher: Option<JoinHandle<()>>,
}

pub enum ScriptStartResult {
    Started,
    AlreadyRunning,
}

pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
