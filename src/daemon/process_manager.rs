use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{ProcessInfo, ProcessStatus};
use crate::daemon::log_monitor::{self, ScriptLogger};
use crate::daemon::process_monitor::ProcessExitEvent;
use crate::daemon::scheduler::{get_scheduler_tx, SchedulerMessage};
use crate::daemon::state::{RunningState, ScriptState};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub struct ManagedProcess {
    pub pid: u32,
    pub command: String,
    pub status: ProcessStatus,
    pub start_time: Option<DateTime<Local>>,
    pub restart_count: u32,
    pub log_file: PathBuf,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub cron: Option<String>,
    logger: Option<ScriptLogger>,
    watcher: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct ProcessManager {
    pub processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    pub state: Arc<Mutex<RunningState>>,
    pub log_dir: PathBuf,
    exit_tx: mpsc::Sender<ProcessExitEvent>,
}

#[derive(Debug)]
pub enum ScriptStartResult {
    Started,
    AlreadyRunning,
    Error(Error),
}

impl ProcessManager {
    pub fn new(
        state_file: PathBuf,
        log_dir: PathBuf,
    ) -> Result<(Self, mpsc::Receiver<ProcessExitEvent>)> {
        tracing::info!(state_file = ?state_file, ?log_dir, "Creating new ProcessManager");
        let state = RunningState::load(&state_file)?;

        tracing::info!(
            scripts_count = state.scripts.len(),
            "State loaded successfully"
        );

        let (exit_tx, exit_rx) = mpsc::channel(256);

        Ok((
            Self {
                processes: Arc::new(Mutex::new(HashMap::new())),
                state: Arc::new(Mutex::new(state)),
                log_dir,
                exit_tx,
            },
            exit_rx,
        ))
    }

    pub async fn restore_state(&self) -> Result<()> {
        tracing::info!("Starting state restoration");
        let scripts_to_restore = {
            let state = self.state.lock().await;
            state.scripts.clone()
        };

        for script in scripts_to_restore.into_iter() {
            if matches!(script.status, ProcessStatus::Running) && !script.explicitly_stopped {
                tracing::info!(script = %script.name, "Restoring script");
                self.start_script(
                    script.name,
                    script.command,
                    script.restart_policy,
                    script.max_restarts,
                    script.cron,
                )
                .await?;
            }
        }
        tracing::info!("State restoration completed");
        Ok(())
    }

    async fn update_script_state(
        &self,
        name: &str,
        status: ProcessStatus,
        explicitly_stopped: bool,
        cron: Option<String>,
    ) -> Result<()> {
        tracing::debug!(script = %name, ?status, explicitly_stopped, "Updating script state");

        let script_state = {
            let processes = self.processes.lock().await;
            let proc = processes
                .get(name)
                .ok_or_else(|| Error::Process(format!("Script {} not found", name)))?;

            ScriptState {
                name: name.to_string(),
                command: proc.command.clone(),
                restart_policy: proc.restart_policy.clone(),
                max_restarts: proc.max_restarts,
                status: status.clone(),
                last_started: proc.start_time,
                last_stopped: if matches!(status, ProcessStatus::Stopped | ProcessStatus::Failed) {
                    Some(Local::now())
                } else {
                    None
                },
                exit_code: None,
                explicitly_stopped,
                cron,
            }
        };

        let mut state = self.state.lock().await;
        state.update_script(script_state).await?;

        tracing::debug!(script = %name, "State updated successfully");
        Ok(())
    }

    pub async fn get_state(&self) -> Vec<ScriptState> {
        let state = self.state.lock().await;
        state.scripts.clone()
    }

    async fn notify_scheduler(&self, msg: SchedulerMessage) {
        match get_scheduler_tx() {
            Some(tx) => {
                tracing::debug!(message = ?msg, "Notifying scheduler");
                if let Err(e) = tx.send(msg).await {
                    tracing::error!(error = ?e, "Failed to notify scheduler");
                }
            }
            None => {
                tracing::error!("Scheduler tx is None, notification skipped");
            }
        }
    }

    pub async fn start_script(
        &self,
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
        cron: Option<String>,
    ) -> Result<ScriptStartResult> {
        tracing::info!(
            script = %name,
            command = %command,
            ?restart_policy,
            max_restarts,
            "Starting script"
        );

        if let Some(result) = self.check_process_alive(&name).await? {
            return Ok(result);
        }

        let log_path = log_monitor::get_log_path(&self.log_dir, &name);
        log_monitor::ensure_log_dir(&self.log_dir)?;

        let logger = ScriptLogger::new(log_path.clone())?;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child.id().unwrap_or(0);

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            log_monitor::spawn_stdout_reader(reader, logger.tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            log_monitor::spawn_stderr_reader(reader, logger.tx.clone());
        }

        let exit_tx = self.exit_tx.clone();
        let watcher_name = name.clone();
        let watcher = tokio::spawn(async move {
            let status = child.wait().await;
            let _ = exit_tx
                .send(ProcessExitEvent {
                    name: watcher_name,
                    status: status.ok(),
                })
                .await;
        });

        let managed_process = ManagedProcess {
            pid,
            command: command.clone(),
            status: ProcessStatus::Running,
            start_time: Some(Local::now()),
            restart_count: 0,
            log_file: log_path,
            restart_policy,
            max_restarts,
            cron: cron.clone(),
            logger: Some(logger),
            watcher: Some(watcher),
        };

        {
            let mut processes = self.processes.lock().await;
            processes.insert(name.clone(), managed_process);
        }

        self.update_script_state(&name, ProcessStatus::Running, false, cron)
            .await?;
        tracing::info!(script = %name, pid, "Script started successfully");
        self.notify_scheduler(SchedulerMessage::ScriptUpdated(name.clone()))
            .await;

        Ok(ScriptStartResult::Started)
    }

    async fn check_process_alive(&self, name: &str) -> Result<Option<ScriptStartResult>> {
        let mut processes = self.processes.lock().await;

        if let Some(process) = processes.get(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                tracing::info!(script = %name, "Script is still running - skipping execution");
                return Ok(Some(ScriptStartResult::AlreadyRunning));
            }
            processes.remove(name);
            tracing::debug!(
                script = %name,
                "Previous process has finished, starting new instance"
            );
        }

        Ok(None)
    }

    pub async fn stop_script(&self, name: &str) -> Result<()> {
        tracing::info!(script = %name, "Stopping script");

        let cron = {
            let state = self.state.lock().await;
            state
                .scripts
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.cron.clone())
                .ok_or_else(|| Error::Process(format!("Script {} not found", name)))?
        };

        self.update_script_state(name, ProcessStatus::Stopped, true, cron)
            .await?;

        let process_opt = {
            let mut processes = self.processes.lock().await;
            processes.remove(name)
        };

        if let Some(mut process) = process_opt {
            if process.pid > 0 && is_process_alive(process.pid) {
                unsafe { libc::kill(process.pid as i32, libc::SIGTERM) };

                if let Some(watcher) = process.watcher.take() {
                    match tokio::time::timeout(Duration::from_secs(5), watcher).await {
                        Ok(_) => {
                            tracing::debug!(script = %name, "Process exited after SIGTERM");
                        }
                        Err(_) => {
                            tracing::warn!(script = %name, "SIGTERM timeout, sending SIGKILL");
                            unsafe { libc::kill(process.pid as i32, libc::SIGKILL) };
                        }
                    }
                }
            } else {
                tracing::debug!(script = %name, "Process already finished");
                if let Some(watcher) = process.watcher.take() {
                    watcher.abort();
                }
            }

            if let Some(logger) = process.logger.take() {
                logger.shutdown();
            }
        }

        tracing::info!(script = %name, "Script stopped successfully");
        self.notify_scheduler(SchedulerMessage::ScriptRemoved(name.to_string()))
            .await;

        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("Shutting down all processes");

        let names = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect::<Vec<_>>()
        };

        for name in names {
            if let Some(mut process) = {
                let mut processes = self.processes.lock().await;
                processes.remove(&name)
            } {
                tracing::debug!(script = %name, "Killing process");

                if process.pid > 0 && is_process_alive(process.pid) {
                    unsafe { libc::kill(process.pid as i32, libc::SIGTERM) };
                    sleep(Duration::from_millis(100)).await;

                    if is_process_alive(process.pid) {
                        unsafe { libc::kill(process.pid as i32, libc::SIGKILL) };
                    }
                }

                if let Some(watcher) = process.watcher.take() {
                    watcher.abort();
                }
                if let Some(logger) = process.logger.take() {
                    logger.shutdown();
                }
            }
        }

        tracing::info!("All processes shut down");
        Ok(())
    }

    pub async fn get_status(&self) -> Result<Vec<ProcessInfo>> {
        let processes = self.processes.lock().await;
        let infos = processes
            .iter()
            .map(|(name, process)| {
                let uptime = process
                    .start_time
                    .map(|start_time| {
                        Local::now()
                            .signed_duration_since(start_time)
                            .to_std()
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                ProcessInfo {
                    name: name.clone(),
                    pid: process.pid,
                    status: process.status.clone(),
                    uptime,
                    restart_count: process.restart_count,
                }
            })
            .collect();
        Ok(infos)
    }

    pub async fn read_logs(&self, name: &str) -> Result<String> {
        log_monitor::ensure_log_dir(&self.log_dir)?;
        let log_path = log_monitor::get_log_path(&self.log_dir, name);

        if !log_path.exists() {
            return Ok("No logs available yet.".to_string());
        }

        let content = tokio::fs::read_to_string(&log_path).await?;
        Ok(content)
    }

    pub async fn stop_all(&self) -> Result<()> {
        let names = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect::<Vec<_>>()
        };
        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    pub async fn read_all_logs(&self) -> Result<String> {
        let log_files: Vec<(String, PathBuf)> = {
            let processes = self.processes.lock().await;
            processes
                .iter()
                .map(|(name, process)| (name.clone(), process.log_file.clone()))
                .collect()
        };

        let mut all_logs = String::new();
        for (name, log_file) in &log_files {
            let log_content = match tokio::fs::read_to_string(log_file).await {
                Ok(logs) => format!("\n=== Logs for {} ===\n{}", name, logs),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    format!("\n=== No logs for {} ===\n", name)
                }
                Err(e) => format!("\n=== Error reading logs for {}: {} ===\n", name, e),
            };
            all_logs.push_str(&log_content);
        }
        Ok(all_logs)
    }
}

fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
