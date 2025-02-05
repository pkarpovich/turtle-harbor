use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{ProcessInfo, ProcessStatus};
use crate::daemon::state::{RunningState, ScriptState, ScriptStatus};

struct ProcessOutput {
    child: Child,
}

pub struct ManagedProcess {
    child: Child,
    command: String,
    status: ProcessStatus,
    start_time: Option<DateTime<Local>>,
    restart_count: u32,
    log_file: PathBuf,
    restart_policy: RestartPolicy,
    max_restarts: u32,
}

pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    state: Arc<Mutex<RunningState>>,
}

impl ProcessManager {
    pub fn new(state_file: PathBuf) -> Result<Self> {
        tracing::info!(state_file = ?state_file, "Creating new ProcessManager");
        let state = RunningState::load(&state_file).map_err(|e| {
            tracing::error!(error = ?e, "Failed to load state");
            Error::Process(format!("Failed to load state: {}", e))
        })?;

        tracing::info!(
            scripts_count = state.scripts.len(),
            "State loaded successfully"
        );

        Ok(Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub async fn restore_state(&self) -> Result<()> {
        tracing::info!("Starting state restoration");
        let scripts_to_restore = {
            let state = self.state.lock().await;
            state.scripts.clone()
        };

        for script in &scripts_to_restore {
            if matches!(script.status, ScriptStatus::Running) && !script.explicitly_stopped {
                tracing::info!(script = %script.name, "Restoring script");
                self.start_script(
                    script.name.clone(),
                    script.command.clone(),
                    script.restart_policy.clone(),
                    script.max_restarts,
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
        status: ScriptStatus,
        explicitly_stopped: bool,
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
                last_stopped: if matches!(status, ScriptStatus::Stopped | ScriptStatus::Failed) {
                    Some(Local::now())
                } else {
                    None
                },
                exit_code: None,
                explicitly_stopped,
            }
        };

        let mut state = self.state.lock().await;
        state
            .update_script(script_state)
            .map_err(|e| Error::Process(format!("Failed to update state: {}", e)))?;

        tracing::debug!(script = %name, "State updated successfully");
        Ok(())
    }

    async fn spawn_log_reader<R>(mut reader: BufReader<R>, file: Arc<Mutex<std::fs::File>>)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
                        let log_line = format!("[{}] {}\n", timestamp, line.trim());
                        let mut f = file.lock().await;
                        use std::io::Write;
                        if let Err(e) = f.write_all(log_line.as_bytes()) {
                            tracing::error!(error = ?e, "Failed to write to log file");
                        }
                        line.clear();
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "Error reading line");
                        break;
                    }
                }
            }
        });
    }

    async fn setup_process_output(command: &str, log_path: &PathBuf) -> Result<ProcessOutput> {
        tracing::debug!(command = %command, path = ?log_path, "Setting up process output");
        Self::ensure_log_dir()?;

        let file = std::fs::OpenOptions::new()
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
            Self::spawn_log_reader(reader, file.clone()).await;
        }
        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            Self::spawn_log_reader(reader, file.clone()).await;
        }

        tracing::debug!("Process output setup completed");
        Ok(ProcessOutput { child })
    }

    pub async fn start_script(
        &self,
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
    ) -> Result<()> {
        tracing::info!(
            script = %name,
            command = %command,
            ?restart_policy,
            max_restarts,
            "Starting script"
        );

        {
            let processes = self.processes.lock().await;
            if processes.contains_key(&name) {
                tracing::info!(script = %name, "Script is already running");
                return Ok(());
            }
        }

        let log_path = Self::get_log_path(&name);
        tracing::debug!(script = %name, log_path = ?log_path, "Setting up process output");

        let output = Self::setup_process_output(&command, &log_path).await?;
        let managed_process = ManagedProcess {
            child: output.child,
            command: command.clone(),
            status: ProcessStatus::Running,
            start_time: Some(Local::now()),
            restart_count: 0,
            log_file: log_path,
            restart_policy,
            max_restarts,
        };

        {
            let mut processes = self.processes.lock().await;
            processes.insert(name.clone(), managed_process);
        }

        self.update_script_state(&name, ScriptStatus::Running, false)
            .await?;
        tracing::info!(script = %name, "Script started successfully");
        Ok(())
    }

    pub async fn stop_script(&self, name: &str) -> Result<()> {
        tracing::info!(script = %name, "Stopping script");
        {
            let mut processes = self.processes.lock().await;
            if let Some(process) = processes.get_mut(name) {
                process.child.kill().await?;
                processes.remove(name);
                tracing::debug!(script = %name, "Process killed and removed");
            } else {
                tracing::error!(script = %name, "Script not found");
                return Err(Error::Process(format!("Script {} not found", name)));
            }
        }

        self.update_script_state(name, ScriptStatus::Stopped, true)
            .await?;
        tracing::info!(script = %name, "Script stopped successfully");
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("Shutting down all processes");
        let names: Vec<String> = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect()
        };

        for name in names {
            tracing::debug!(script = %name, "Killing process");
            let mut processes = self.processes.lock().await;
            if let Some(process) = processes.get_mut(&name) {
                process.child.kill().await?;
            }
        }

        tracing::info!("All processes shut down");
        Ok(())
    }

    pub async fn get_status(&self) -> Result<Vec<ProcessInfo>> {
        let processes = self.processes.lock().await;
        Ok(processes
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
            .collect())
    }

    pub async fn read_logs(&self, name: &str) -> Result<String> {
        Self::ensure_log_dir()?;

        let log_path = Self::get_log_path(name);
        if !log_path.exists() {
            return Ok("No logs available yet.".to_string());
        }

        let processes = self.processes.lock().await;
        match processes.get(name) {
            Some(process) => Ok(tokio::fs::read_to_string(&process.log_file).await?),
            None => Err(Error::Process(format!("Script {} not found", name))),
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let names: Vec<String> = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect()
        };
        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    pub async fn read_all_logs(&self) -> Result<String> {
        let processes = self.processes.lock().await;
        let mut all_logs = String::new();
        for (name, process) in processes.iter() {
            let log_content = match tokio::fs::read_to_string(&process.log_file).await {
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

    async fn should_restart(&self, name: &str) -> Option<(String, RestartPolicy, u32)> {
        let mut processes = self.processes.lock().await;
        processes.get_mut(name).and_then(|process| {
            if let Ok(Some(_)) = process.child.try_wait() {
                match process.restart_policy {
                    RestartPolicy::Always if process.restart_count < process.max_restarts => {
                        process.restart_count += 1;
                        tracing::info!(
                            script = %name,
                            restart_count = process.restart_count,
                            max_restarts = process.max_restarts,
                            "Restarting process"
                        );
                        Some((
                            process.command.clone(),
                            process.restart_policy.clone(),
                            process.max_restarts,
                        ))
                    }
                    _ => {
                        tracing::debug!(script = %name, "Process will not be restarted");
                        None
                    }
                }
            } else {
                None
            }
        })
    }

    pub async fn check_and_restart_if_needed(&self, name: &str) {
        if let Some((command, policy, max_restarts)) = self.should_restart(name).await {
            if let Err(e) = self
                .start_script(name.to_string(), command, policy, max_restarts)
                .await
            {
                tracing::error!(script = %name, error = ?e, "Failed to restart script");
            }
        }
    }

    pub async fn monitor_and_restart(&self) {
        tracing::info!("Starting process monitor");
        loop {
            let names: Vec<String> = {
                let processes = self.processes.lock().await;
                processes.keys().cloned().collect()
            };
            for name in names {
                self.check_and_restart_if_needed(&name).await;
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    fn get_log_path(name: &str) -> PathBuf {
        let path = PathBuf::from("logs").join(format!("{}.log", name));
        tracing::trace!(script = %name, path = ?path, "Generated log path");
        path
    }

    fn ensure_log_dir() -> Result<()> {
        let log_dir = PathBuf::from("logs");
        if !log_dir.exists() {
            tracing::debug!(path = ?log_dir, "Creating log directory");
            std::fs::create_dir_all(&log_dir)?;
        }
        Ok(())
    }
}
