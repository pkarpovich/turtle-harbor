use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{Command, ProcessInfo, ProcessStatus, Response};
use crate::daemon::log_monitor::{self, ScriptLogger};
use crate::daemon::process::{is_process_alive, ManagedProcess, ScriptStartResult};
use crate::daemon::scheduler;
use crate::daemon::state::{RunningState, ScriptState};
use chrono::Local;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

pub enum DaemonEvent {
    ClientCommand {
        command: Command,
        reply_tx: oneshot::Sender<Response>,
    },
    ProcessExited {
        name: String,
        status: Option<ExitStatus>,
    },
    CronTick {
        name: String,
    },
    Shutdown,
}

pub struct DaemonCore {
    processes: HashMap<String, ManagedProcess>,
    state: RunningState,
    log_dir: PathBuf,
    event_tx: mpsc::Sender<DaemonEvent>,
    event_rx: mpsc::Receiver<DaemonEvent>,
    cron_tasks: HashMap<String, JoinHandle<()>>,
}

impl DaemonCore {
    pub fn new(state_file: PathBuf, log_dir: PathBuf) -> Result<Self> {
        tracing::info!(state_file = ?state_file, ?log_dir, "Creating DaemonCore");
        let state = RunningState::load(&state_file)?;
        tracing::info!(scripts_count = state.scripts.len(), "State loaded");

        let (event_tx, event_rx) = mpsc::channel(256);

        Ok(Self {
            processes: HashMap::new(),
            state,
            log_dir,
            event_tx,
            event_rx,
            cron_tasks: HashMap::new(),
        })
    }

    pub fn event_tx(&self) -> mpsc::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    pub async fn run(&mut self) -> Result<()> {
        self.restore_state().await?;
        tracing::info!("Daemon core event loop started");

        while let Some(event) = self.event_rx.recv().await {
            match event {
                DaemonEvent::ClientCommand { command, reply_tx } => {
                    let response = self.handle_command(command).await;
                    let _ = reply_tx.send(response);
                }
                DaemonEvent::ProcessExited { name, status } => {
                    self.handle_process_exit(&name, status).await;
                }
                DaemonEvent::CronTick { name } => {
                    self.handle_cron_tick(&name).await;
                }
                DaemonEvent::Shutdown => {
                    tracing::info!("Shutdown event received");
                    break;
                }
            }
        }

        self.shutdown().await
    }

    async fn handle_command(&mut self, command: Command) -> Response {
        match command {
            Command::Up {
                name,
                command,
                restart_policy,
                max_restarts,
                cron,
            } => {
                match self
                    .start_script(name, command, restart_policy, max_restarts, cron)
                    .await
                {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::Down { name } => match self.stop_script(&name).await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Ps => match self.get_status() {
                Ok(status) => Response::ProcessList(status),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Logs { name } => match self.read_logs(&name).await {
                Ok(logs) => Response::Logs(logs),
                Err(e) => Response::Error(e.to_string()),
            },
        }
    }

    async fn handle_process_exit(&mut self, name: &str, status: Option<ExitStatus>) {
        let should_restart = match &status {
            Some(s) if s.success() => {
                tracing::info!(script = %name, "Process exited successfully");
                false
            }
            Some(s) => {
                let code = s.code().unwrap_or(-1);
                tracing::warn!(script = %name, exit_code = code, "Process exited with error");
                self.processes
                    .get(name)
                    .map(|p| {
                        matches!(&p.restart_policy, RestartPolicy::Always)
                            && p.restart_count < p.max_restarts
                    })
                    .unwrap_or(false)
            }
            None => {
                tracing::info!(script = %name, "Process terminated by signal");
                false
            }
        };

        if should_restart {
            let restart_info = self.processes.get_mut(name).map(|p| {
                p.restart_count += 1;
                tracing::info!(
                    script = %name,
                    restart_count = p.restart_count,
                    max_restarts = p.max_restarts,
                    "Initiating restart"
                );
                (
                    p.command.clone(),
                    p.restart_policy.clone(),
                    p.max_restarts,
                    p.cron.clone(),
                )
            });

            self.cleanup_process(name);

            if let Some((command, policy, max_restarts, cron)) = restart_info {
                if let Err(e) = self
                    .start_script(name.to_string(), command, policy, max_restarts, cron)
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to restart");
                }
            }
        } else {
            self.cleanup_process(name);
        }
    }

    async fn handle_cron_tick(&mut self, name: &str) {
        let script_info = self
            .state
            .scripts
            .iter()
            .find(|s| s.name == name && s.cron.is_some())
            .cloned();

        if let Some(script) = script_info {
            match self
                .start_script(
                    script.name.clone(),
                    script.command,
                    script.restart_policy,
                    script.max_restarts,
                    script.cron,
                )
                .await
            {
                Ok(ScriptStartResult::Started) => {
                    tracing::info!(script = %name, "Cron-triggered script started");
                }
                Ok(ScriptStartResult::AlreadyRunning) => {
                    tracing::debug!(script = %name, "Cron tick - script already running");
                }
                Err(e) => {
                    tracing::error!(script = %name, error = ?e, "Cron-triggered start failed");
                }
            }
        }
    }

    async fn start_script(
        &mut self,
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

        if let Some(process) = self.processes.get(&name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                tracing::info!(script = %name, "Script is still running - skipping");
                return Ok(ScriptStartResult::AlreadyRunning);
            }
        }
        self.cleanup_process(&name);

        let log_path = log_monitor::get_log_path(&self.log_dir, &name);
        log_monitor::ensure_log_dir(&self.log_dir)?;
        let logger = ScriptLogger::new(log_path)?;

        let mut child = TokioCommand::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child.id().unwrap_or(0);

        if let Some(stdout) = child.stdout.take() {
            log_monitor::spawn_stdout_reader(BufReader::new(stdout), logger.tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            log_monitor::spawn_stderr_reader(BufReader::new(stderr), logger.tx.clone());
        }

        let event_tx = self.event_tx.clone();
        let watcher_name = name.clone();
        let watcher = tokio::spawn(async move {
            let status = child.wait().await;
            let _ = event_tx
                .send(DaemonEvent::ProcessExited {
                    name: watcher_name,
                    status: status.ok(),
                })
                .await;
        });

        self.processes.insert(
            name.clone(),
            ManagedProcess {
                pid,
                command: command.clone(),
                status: ProcessStatus::Running,
                start_time: Some(Local::now()),
                restart_count: 0,
                restart_policy,
                max_restarts,
                cron: cron.clone(),
                logger: Some(logger),
                watcher: Some(watcher),
            },
        );

        self.update_script_state(&name, ProcessStatus::Running, false, cron.clone())
            .await?;

        if let Some(ref cron_expr) = cron {
            if !self.cron_tasks.contains_key(&name) {
                self.schedule_cron(&name, cron_expr);
            }
        }

        tracing::info!(script = %name, pid, "Script started successfully");
        Ok(ScriptStartResult::Started)
    }

    async fn stop_script(&mut self, name: &str) -> Result<()> {
        tracing::info!(script = %name, "Stopping script");

        let cron = self
            .state
            .scripts
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.cron.clone())
            .ok_or_else(|| Error::Process(format!("Script {} not found", name)))?;

        self.update_script_state(name, ProcessStatus::Stopped, true, cron)
            .await?;

        if let Some(mut process) = self.processes.remove(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                unsafe { libc::kill(process.pid as i32, libc::SIGTERM) };

                if let Some(watcher) = process.watcher.take() {
                    match tokio::time::timeout(Duration::from_secs(5), watcher).await {
                        Ok(_) => tracing::debug!(script = %name, "Process exited after SIGTERM"),
                        Err(_) => {
                            tracing::warn!(script = %name, "SIGTERM timeout, sending SIGKILL");
                            unsafe { libc::kill(process.pid as i32, libc::SIGKILL) };
                        }
                    }
                }
            } else {
                if let Some(watcher) = process.watcher.take() {
                    watcher.abort();
                }
            }

            if let Some(logger) = process.logger.take() {
                logger.shutdown();
            }
        }

        self.cancel_cron(name);
        tracing::info!(script = %name, "Script stopped successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        tracing::info!("Shutting down all processes");

        let names: Vec<String> = self.processes.keys().cloned().collect();
        for name in names {
            if let Some(mut process) = self.processes.remove(&name) {
                tracing::debug!(script = %name, "Killing process");

                if process.pid > 0 && is_process_alive(process.pid) {
                    unsafe { libc::kill(process.pid as i32, libc::SIGTERM) };
                    tokio::time::sleep(Duration::from_millis(100)).await;

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

        for (_, handle) in self.cron_tasks.drain() {
            handle.abort();
        }

        tracing::info!("All processes shut down");
        Ok(())
    }

    fn get_status(&self) -> Result<Vec<ProcessInfo>> {
        let infos = self
            .processes
            .iter()
            .map(|(name, process)| {
                let uptime = process
                    .start_time
                    .map(|st| {
                        Local::now()
                            .signed_duration_since(st)
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

    async fn read_logs(&self, name: &str) -> Result<String> {
        log_monitor::ensure_log_dir(&self.log_dir)?;
        let log_path = log_monitor::get_log_path(&self.log_dir, name);

        if !log_path.exists() {
            return Ok("No logs available yet.".to_string());
        }

        let content = tokio::fs::read_to_string(&log_path).await?;
        Ok(content)
    }

    async fn restore_state(&mut self) -> Result<()> {
        tracing::info!("Starting state restoration");
        let scripts: Vec<ScriptState> = self.state.scripts.clone();

        for script in scripts {
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
        &mut self,
        name: &str,
        status: ProcessStatus,
        explicitly_stopped: bool,
        cron: Option<String>,
    ) -> Result<()> {
        let proc = self
            .processes
            .get(name)
            .ok_or_else(|| Error::Process(format!("Script {} not found", name)))?;

        let script_state = ScriptState {
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
        };

        self.state.update_script(script_state).await?;
        tracing::debug!(script = %name, ?status, "State updated");
        Ok(())
    }

    fn schedule_cron(&mut self, name: &str, cron_expr: &str) {
        self.cancel_cron(name);

        match scheduler::spawn_cron_task(name.to_string(), cron_expr, self.event_tx.clone()) {
            Ok(handle) => {
                self.cron_tasks.insert(name.to_string(), handle);
                tracing::info!(script = %name, "Cron task scheduled");
            }
            Err(e) => {
                tracing::error!(script = %name, error = ?e, "Failed to schedule cron");
            }
        }
    }

    fn cancel_cron(&mut self, name: &str) {
        if let Some(handle) = self.cron_tasks.remove(name) {
            handle.abort();
            tracing::debug!(script = %name, "Cron task cancelled");
        }
    }

    fn cleanup_process(&mut self, name: &str) {
        if let Some(mut process) = self.processes.remove(name) {
            if let Some(watcher) = process.watcher.take() {
                watcher.abort();
            }
            if let Some(logger) = process.logger.take() {
                logger.shutdown();
            }
        }
    }
}
