use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{ProcessInfo, ProcessStatus};
use crate::daemon::log_monitor;
use crate::daemon::scheduler::{get_scheduler_tx, SchedulerMessage};
use crate::daemon::state::{RunningState, ScriptState};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

pub struct ManagedProcess {
    pub child: Child,
    pub command: String,
    pub status: ProcessStatus,
    pub start_time: Option<DateTime<Local>>,
    pub restart_count: u32,
    pub log_file: PathBuf,
    pub restart_policy: RestartPolicy,
    pub max_restarts: u32,
    pub cron: Option<String>,
}

pub struct ProcessManager {
    pub processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    pub state: Arc<Mutex<RunningState>>,
    pub log_dir: PathBuf,
}

#[derive(Debug)]
pub enum ScriptStartResult {
    Started,
    AlreadyRunning,
    Error(Error),
}

impl ProcessManager {
    pub fn new(state_file: PathBuf, log_dir: PathBuf) -> Result<Self> {
        tracing::info!(state_file = ?state_file, ?log_dir, "Creating new ProcessManager");
        let state = RunningState::load(&state_file)?;

        tracing::info!(
            scripts_count = state.scripts.len(),
            "State loaded successfully"
        );

        Ok(Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(state)),
            log_dir,
        })
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
        state.update_script(script_state)?;

        tracing::debug!(script = %name, "State updated successfully");
        Ok(())
    }

    pub async fn get_state(&self) -> Vec<ScriptState> {
        let state = self.state.lock().await;
        state.scripts.clone()
    }

    pub fn get_processes(&self) -> Arc<Mutex<HashMap<String, ManagedProcess>>> {
        Arc::clone(&self.processes)
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

    async fn setup_process_output(command: &str, log_path: &PathBuf, log_dir: &Path) -> Result<Child> {
        tracing::debug!(command = %command, path = ?log_path, "Setting up process output");
        log_monitor::ensure_log_dir(log_dir)?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let file = Arc::new(Mutex::new(file));

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            log_monitor::spawn_log_reader(reader, file.clone()).await;
        }
        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            log_monitor::spawn_log_reader(reader, file.clone()).await;
        }

        tracing::debug!("Process output setup completed");
        Ok(child)
    }

    async fn check_process_status(&self, name: &str) -> Result<Option<ScriptStartResult>> {
        let mut processes = self.processes.lock().await;

        if let Some(process) = processes.get_mut(name) {
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    processes.remove(name);
                    tracing::debug!(
                        script = %name,
                        exit_status = ?status,
                        "Previous process has finished, starting new instance"
                    );
                    Ok(None)
                }
                Ok(None) => {
                    tracing::info!(script = %name, "Script is still running - skipping execution");
                    Ok(Some(ScriptStartResult::AlreadyRunning))
                }
                Err(e) => {
                    tracing::error!(
                        script = %name,
                        error = ?e,
                        "Error checking process status"
                    );
                    processes.remove(name);
                    Err(Error::Process(format!(
                        "Error checking process status: {}",
                        e
                    )))
                }
            }
        } else {
            Ok(None)
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

        if let Some(result) = self.check_process_status(&name).await? {
            return Ok(result);
        }

        let log_path = log_monitor::get_log_path(&self.log_dir, &name);
        let child = Self::setup_process_output(&command, &log_path, &self.log_dir).await?;
        let managed_process = ManagedProcess {
            child,
            command: command.clone(),
            status: ProcessStatus::Running,
            start_time: Some(Local::now()),
            restart_count: 0,
            log_file: log_path,
            restart_policy,
            max_restarts,
            cron: cron.clone(),
        };

        {
            let mut processes = self.processes.lock().await;
            processes.insert(name.clone(), managed_process);
        }

        self.update_script_state(&name, ProcessStatus::Running, false, cron)
            .await?;
        tracing::info!(script = %name, "Script started successfully");
        self.notify_scheduler(SchedulerMessage::ScriptUpdated(name.clone()))
            .await;

        Ok(ScriptStartResult::Started)
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
            match process.child.try_wait() {
                Ok(Some(_)) => {
                    tracing::debug!(script = %name, "Process already finished");
                }
                Ok(None) => match timeout(Duration::from_secs(5), process.child.kill()).await {
                    Ok(kill_result) => {
                        kill_result?;
                        tracing::debug!(script = %name, "Process killed");
                    }
                    Err(_) => {
                        tracing::warn!(script = %name, "Kill operation timed out, forcing kill");
                        process.child.start_kill()?;
                    }
                },
                Err(e) => {
                    tracing::error!(script = %name, error = ?e, "Error checking process status");
                }
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
                process.child.kill().await?;
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
                    pid: process.child.id().unwrap_or(0),
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

impl Clone for ProcessManager {
    fn clone(&self) -> Self {
        Self {
            processes: Arc::clone(&self.processes),
            state: Arc::clone(&self.state),
            log_dir: self.log_dir.clone(),
        }
    }
}
