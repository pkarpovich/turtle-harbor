use crate::common::config::{Config, RestartPolicy};
use crate::common::error::{Error, Result};
use crate::common::ipc::{Command, ProcessInfo, ProcessStatus, Response};
use crate::daemon::log_monitor::{self, ScriptLogger};
use crate::daemon::process::{is_process_alive, ManagedProcess, ScriptStartResult};
use crate::daemon::scheduler;
use crate::daemon::state::{RunningState, ScriptState};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    config: Option<Config>,
    config_path: Option<PathBuf>,
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
            config: None,
            config_path: None,
            log_dir,
            event_tx,
            event_rx,
            cron_tasks: HashMap::new(),
        })
    }

    pub fn event_tx(&self) -> mpsc::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
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

    fn load_config(&mut self, config_path: &Path) -> Result<()> {
        let config = Config::load(config_path)?;
        self.log_dir = config.settings.log_dir.clone();
        self.config = Some(config);
        self.config_path = Some(config_path.to_path_buf());
        self.state.config_path = Some(config_path.to_path_buf());
        Ok(())
    }

    fn require_config(&self) -> Result<&Config> {
        self.config.as_ref().ok_or(Error::ConfigNotLoaded)
    }

    async fn handle_command(&mut self, command: Command) -> Response {
        match command {
            Command::Up { name, config_path } => {
                if let Err(e) = self.load_config(&config_path) {
                    return Response::Error(e.to_string());
                }
                match self.start_scripts(name).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::Down { name } => match self.stop_scripts(name).await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Ps => match self.get_status() {
                Ok(status) => Response::ProcessList(status),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Logs { name, tail, follow: _ } => match self.read_logs(name.as_deref(), tail).await {
                Ok(logs) => Response::Logs(logs),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Reload => match self.reload_config().await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
        }
    }

    async fn handle_process_exit(&mut self, name: &str, status: Option<ExitStatus>) {
        let script_def = self.config.as_ref().and_then(|c| c.scripts.get(name));
        let should_restart = match &status {
            Some(s) if s.success() => {
                tracing::info!(script = %name, "Process exited successfully");
                false
            }
            Some(s) => {
                let code = s.code().unwrap_or(-1);
                tracing::warn!(script = %name, exit_code = code, "Process exited with error");
                match (script_def, self.processes.get(name)) {
                    (Some(def), Some(proc)) => {
                        matches!(def.restart_policy, RestartPolicy::Always)
                            && proc.restart_count < def.max_restarts
                    }
                    _ => false,
                }
            }
            None => {
                tracing::info!(script = %name, "Process terminated by signal");
                false
            }
        };

        if should_restart {
            let restart_count = self.processes.get_mut(name).map(|p| {
                p.restart_count += 1;
                tracing::info!(
                    script = %name,
                    restart_count = p.restart_count,
                    "Initiating restart"
                );
                p.restart_count
            });

            self.cleanup_process(name);

            if let Some(count) = restart_count {
                if let Err(e) = self.start_script(name).await {
                    tracing::error!(script = %name, error = ?e, "Failed to restart");
                } else if let Some(proc) = self.processes.get_mut(name) {
                    proc.restart_count = count;
                }
            }
        } else {
            self.cleanup_process(name);
        }
    }

    async fn handle_cron_tick(&mut self, name: &str) {
        let has_script = self
            .config
            .as_ref()
            .map_or(false, |c| c.scripts.contains_key(name));
        if !has_script {
            return;
        }

        match self.start_script(name).await {
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

    async fn start_scripts(&mut self, name: Option<String>) -> Result<()> {
        match name {
            Some(name) => {
                self.start_script(&name).await?;
            }
            None => {
                let names: Vec<String> = self
                    .require_config()?
                    .scripts
                    .keys()
                    .cloned()
                    .collect();
                for name in names {
                    self.start_script(&name).await?;
                }
            }
        }
        Ok(())
    }

    async fn stop_scripts(&mut self, name: Option<String>) -> Result<()> {
        match name {
            Some(name) => {
                self.stop_script(&name).await?;
            }
            None => {
                let names: Vec<String> = self.processes.keys().cloned().collect();
                for name in names {
                    self.stop_script(&name).await?;
                }
            }
        }
        Ok(())
    }

    async fn start_script(&mut self, name: &str) -> Result<ScriptStartResult> {
        let script_def = self
            .require_config()?
            .scripts
            .get(name)
            .ok_or_else(|| Error::ScriptNotFound {
                name: name.to_string(),
            })?
            .clone();
        let command = script_def.command;
        let cron = script_def.cron;

        tracing::info!(
            script = %name,
            command = %command,
            "Starting script"
        );

        if let Some(process) = self.processes.get(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                tracing::info!(script = %name, "Script is still running - skipping");
                return Ok(ScriptStartResult::AlreadyRunning);
            }
        }
        self.cleanup_process(name);

        let canonical_command = std::fs::canonicalize(&command)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(command);

        let log_path = log_monitor::get_log_path(&self.log_dir, name);
        log_monitor::ensure_log_dir(&self.log_dir)?;
        let logger = ScriptLogger::new(log_path)?;

        let mut child = unsafe {
            TokioCommand::new("sh")
                .arg("-c")
                .arg(&canonical_command)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()?
        };

        let pid = child.id().unwrap_or(0);

        if let Some(stdout) = child.stdout.take() {
            log_monitor::spawn_stdout_reader(BufReader::new(stdout), logger.tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            log_monitor::spawn_stderr_reader(BufReader::new(stderr), logger.tx.clone());
        }

        let event_tx = self.event_tx.clone();
        let watcher_name = name.to_string();
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
            name.to_string(),
            ManagedProcess {
                pid,
                status: ProcessStatus::Running,
                start_time: Some(Local::now()),
                restart_count: 0,
                logger: Some(logger),
                watcher: Some(watcher),
            },
        );

        self.update_script_state(name, ProcessStatus::Running, false)
            .await?;

        if let Some(ref cron_expr) = cron {
            if !self.cron_tasks.contains_key(name) {
                self.schedule_cron(name, cron_expr);
            }
        }

        tracing::info!(script = %name, pid, "Script started successfully");
        Ok(ScriptStartResult::Started)
    }

    async fn stop_script(&mut self, name: &str) -> Result<()> {
        tracing::info!(script = %name, "Stopping script");

        if !self.processes.contains_key(name)
            && !self.state.scripts.iter().any(|s| s.name == name)
        {
            return Err(Error::ScriptNotFound {
                name: name.to_string(),
            });
        }

        if self.processes.contains_key(name) {
            self.update_script_state(name, ProcessStatus::Stopped, true)
                .await?;
        }

        if let Some(mut process) = self.processes.remove(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                let pgid = process.pid as i32;
                unsafe { libc::killpg(pgid, libc::SIGTERM) };

                if let Some(watcher) = process.watcher.take() {
                    match tokio::time::timeout(Duration::from_secs(5), watcher).await {
                        Ok(_) => tracing::debug!(script = %name, "Process group exited after SIGTERM"),
                        Err(_) => {
                            tracing::warn!(script = %name, "SIGTERM timeout, sending SIGKILL to process group");
                            unsafe { libc::killpg(pgid, libc::SIGKILL) };
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
                    let pgid = process.pid as i32;
                    unsafe { libc::killpg(pgid, libc::SIGTERM) };
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    if is_process_alive(process.pid) {
                        unsafe { libc::killpg(pgid, libc::SIGKILL) };
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

    async fn read_logs(&self, name: Option<&str>, tail: u32) -> Result<String> {
        log_monitor::ensure_log_dir(&self.log_dir)?;

        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => log_monitor::list_script_names(&self.log_dir),
        };

        if names.is_empty() {
            return Ok("No logs available yet.".to_string());
        }

        let mut output = String::new();
        for (i, script_name) in names.iter().enumerate() {
            let log_path = log_monitor::get_log_path(&self.log_dir, script_name);
            if !log_path.exists() {
                continue;
            }
            if names.len() > 1 {
                if i > 0 {
                    output.push('\n');
                }
                output.push_str(&format!("=== {} ===\n", script_name));
            }
            let content = log_monitor::read_last_n_lines(&log_path, tail)?;
            output.push_str(&content);
        }

        if output.is_empty() {
            return Ok("No logs available yet.".to_string());
        }
        Ok(output)
    }

    async fn restore_state(&mut self) -> Result<()> {
        tracing::info!("Starting state restoration");

        if let Some(config_path) = self.state.config_path.clone() {
            if config_path.exists() {
                tracing::info!(config = ?config_path, "Loading config from state");
                if let Err(e) = self.load_config(&config_path) {
                    tracing::warn!(error = ?e, "Failed to load config from state, skipping restore");
                    return Ok(());
                }
            } else {
                tracing::warn!(config = ?config_path, "Stored config path no longer exists, skipping restore");
                return Ok(());
            }
        } else {
            tracing::info!("No config path in state, skipping restore");
            return Ok(());
        }

        let scripts: Vec<ScriptState> = self.state.scripts.clone();
        for script in scripts {
            if !matches!(script.status, ProcessStatus::Running) || script.explicitly_stopped {
                continue;
            }
            let in_config = self
                .config
                .as_ref()
                .map_or(false, |c| c.scripts.contains_key(&script.name));
            if !in_config {
                tracing::info!(script = %script.name, "Skipping restoration - no longer in config");
                continue;
            }
            tracing::info!(script = %script.name, "Restoring script");
            self.start_script(&script.name).await?;
        }

        let cron_entries: Vec<(String, String)> = self
            .config
            .as_ref()
            .map(|c| {
                c.scripts
                    .iter()
                    .filter(|(name, def)| def.cron.is_some() && !self.cron_tasks.contains_key(*name))
                    .filter_map(|(name, def)| {
                        def.cron.as_ref().map(|expr| (name.clone(), expr.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (name, cron_expr) in cron_entries {
            self.schedule_cron(&name, &cron_expr);
        }

        tracing::info!("State restoration completed");
        Ok(())
    }

    async fn update_script_state(
        &mut self,
        name: &str,
        status: ProcessStatus,
        explicitly_stopped: bool,
    ) -> Result<()> {
        let restart_count = self.processes.get(name).map(|p| p.restart_count).unwrap_or(0);
        let start_time = self.processes.get(name).and_then(|p| p.start_time);

        let script_state = ScriptState {
            name: name.to_string(),
            status: status.clone(),
            last_started: start_time,
            last_stopped: if matches!(status, ProcessStatus::Stopped | ProcessStatus::Failed) {
                Some(Local::now())
            } else {
                None
            },
            exit_code: None,
            explicitly_stopped,
            restart_count,
        };

        self.state.update_script(script_state).await?;
        tracing::debug!(script = %name, ?status, "State updated");
        Ok(())
    }

    async fn reload_config(&mut self) -> Result<()> {
        let config_path = self
            .config_path
            .clone()
            .ok_or(Error::ConfigNotLoaded)?;

        tracing::info!(config = ?config_path, "Reloading configuration");
        let new_config = Config::load(&config_path)?;
        let old_config = self.config.replace(new_config);
        self.log_dir = self.config.as_ref().unwrap().settings.log_dir.clone();

        let Some(old_config) = old_config else {
            return Ok(());
        };

        let old_names: HashSet<String> = old_config.scripts.keys().cloned().collect();
        let new_names: HashSet<String> = self
            .config
            .as_ref()
            .unwrap()
            .scripts
            .keys()
            .cloned()
            .collect();

        let removed: Vec<String> = old_names.difference(&new_names).cloned().collect();
        for name in removed {
            tracing::info!(script = %name, "Script removed from config, stopping");
            let _ = self.stop_script(&name).await;
            self.state.remove_script(&name).await?;
        }

        let added: Vec<String> = new_names.difference(&old_names).cloned().collect();
        for name in added {
            tracing::info!(script = %name, "New script in config, starting");
            let _ = self.start_script(&name).await;
        }

        let new_scripts = &self.config.as_ref().unwrap().scripts;
        let changed: Vec<String> = old_names
            .intersection(&new_names)
            .filter(|name| old_config.scripts[*name] != new_scripts[*name])
            .cloned()
            .collect();
        for name in changed {
            tracing::info!(script = %name, "Script config changed, restarting");
            let _ = self.stop_script(&name).await;
            self.cancel_cron(&name);
            let _ = self.start_script(&name).await;
        }

        tracing::info!("Configuration reloaded");
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
