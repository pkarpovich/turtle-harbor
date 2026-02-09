use crate::common::config::RestartPolicy;
use crate::common::ipc::ProcessStatus;
use crate::daemon::log_monitor::ScriptLogger;
use chrono::{DateTime, Local};
use tokio::task::JoinHandle;

pub struct ManagedProcess {
    pub pid: u32,
    pub command: String,
    pub status: ProcessStatus,
    pub start_time: Option<DateTime<Local>>,
    pub restart_count: u32,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub cron: Option<String>,
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
    // SAFETY: kill with signal 0 performs an existence check without sending a signal.
    // pid is a valid u32 from child.id(), cast to i32 is safe for valid PIDs.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
