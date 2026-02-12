use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{Command, ProcessStatus, Response};
use crate::daemon::config_manager::ConfigManager;
use crate::daemon::cron_manager::CronManager;
use crate::daemon::health::{self, HealthSnapshot, ScriptHealth, ScriptHealthState};
use crate::daemon::log_monitor;
use crate::daemon::loki_shipper::{self, LokiLogEntry, LokiShipper};
use crate::daemon::process::ScriptStartResult;
use crate::daemon::process_supervisor::ProcessSupervisor;
use crate::daemon::state::{RunningState, ScriptState};
use chrono::Local;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

pub type LogChannels = Arc<Mutex<HashMap<String, broadcast::Sender<String>>>>;

pub enum DaemonEvent {
    ClientCommand {
        command: Command,
        reply_tx: oneshot::Sender<Response>,
    },
    ProcessExited {
        name: String,
        status: Option<ExitStatus>,
    },
    RestartAfterBackoff {
        name: String,
        restart_count: u32,
    },
    CronTick {
        name: String,
    },
    Shutdown,
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_secs: u64 = 15;
    let multiplier = 1u64.checked_shl(attempt.saturating_sub(1)).unwrap_or(u64::MAX);
    let secs = base_secs.saturating_mul(multiplier).min(300);
    Duration::from_secs(secs)
}

pub struct DaemonCore {
    supervisor: ProcessSupervisor,
    config: ConfigManager,
    cron: CronManager,
    state: RunningState,
    event_tx: mpsc::Sender<DaemonEvent>,
    event_rx: mpsc::Receiver<DaemonEvent>,
    log_channels: LogChannels,
    health: HealthSnapshot,
    loki_tx: Option<mpsc::Sender<LokiLogEntry>>,
}

impl DaemonCore {
    pub fn new(state_file: PathBuf, log_dir: PathBuf) -> Result<Self> {
        tracing::info!(state_file = ?state_file, ?log_dir, "Creating DaemonCore");
        let state = RunningState::load(&state_file)?;
        tracing::info!(scripts_count = state.scripts.len(), "State loaded");

        let (event_tx, event_rx) = mpsc::channel(256);

        let supervisor = ProcessSupervisor::new(event_tx.clone(), log_dir);
        let config = ConfigManager::new();
        let cron = CronManager::new(event_tx.clone());
        let log_channels = Arc::new(Mutex::new(HashMap::new()));
        let health = health::new_health_snapshot();

        Ok(Self {
            supervisor,
            config,
            cron,
            state,
            event_tx,
            event_rx,
            log_channels,
            health,
            loki_tx: None,
        })
    }

    pub fn event_tx(&self) -> mpsc::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    pub fn log_channels(&self) -> LogChannels {
        self.log_channels.clone()
    }

    pub fn health_snapshot(&self) -> HealthSnapshot {
        self.health.clone()
    }

    fn register_log_channel(&self, name: &str) -> broadcast::Sender<String> {
        let (tx, _) = broadcast::channel(256);
        self.log_channels
            .lock()
            .expect("log_channels mutex poisoned")
            .insert(name.to_string(), tx.clone());
        tx
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
                DaemonEvent::RestartAfterBackoff {
                    name,
                    restart_count,
                } => {
                    self.handle_restart_after_backoff(&name, restart_count)
                        .await;
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

    fn sync_settings(&mut self) {
        if let Some(log_dir) = self.config.log_dir() {
            self.supervisor.set_log_dir(log_dir.to_path_buf());
        }

        if self.loki_tx.is_none() {
            if let Some(loki_config) = self.config.loki_config().cloned() {
                let (tx, rx) = mpsc::channel(loki_shipper::CHANNEL_CAPACITY);
                LokiShipper::spawn(rx, loki_config);
                self.supervisor.set_loki_tx(tx.clone());
                self.loki_tx = Some(tx);
            }
        }
    }

    async fn handle_command(&mut self, command: Command) -> Response {
        match command {
            Command::Up { name, config_path } => {
                if let Err(e) = self.config.load(&config_path) {
                    return Response::Error(e.to_string());
                }
                self.sync_settings();
                self.state.config_path = Some(config_path);
                match self.start_scripts(name).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::Down { name } => match self.stop_scripts(name).await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Ps => Response::ProcessList(self.supervisor.status_list()),
            Command::Logs {
                name,
                tail,
                follow: _,
            } => match self.read_logs(name.as_deref(), tail).await {
                Ok(logs) => Response::Logs(logs),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Reload => match self.reload_config().await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
        }
    }

    fn should_restart(&self, name: &str, status: &Option<ExitStatus>) -> bool {
        let Some(exit_status) = status else {
            tracing::info!(script = %name, "Process terminated by signal");
            return false;
        };

        if exit_status.success() {
            tracing::info!(script = %name, "Process exited successfully");
            return false;
        }

        let code = exit_status.code().unwrap_or(-1);
        tracing::warn!(script = %name, exit_code = code, "Process exited with error");

        match (self.config.script(name), self.supervisor.get(name)) {
            (Some(def), Some(proc)) => {
                matches!(def.restart_policy, RestartPolicy::Always)
                    && proc.restart_count < def.effective_max_restarts()
            }
            _ => false,
        }
    }

    async fn handle_process_exit(&mut self, name: &str, status: Option<ExitStatus>) {
        let exit_code = status.and_then(|s| s.code());
        let succeeded = status.map(|s| s.success()).unwrap_or(false);

        if succeeded {
            if let Some(proc) = self.supervisor.get_mut(name) {
                proc.restart_count = 0;
            }
        }

        {
            let mut snapshot = self.health.write().await;
            if let Some(entry) = snapshot.get_mut(name) {
                entry.state = if succeeded {
                    ScriptHealthState::Succeeded
                } else {
                    ScriptHealthState::Failed
                };
                entry.healthy = succeeded;
                entry.last_exit_code = exit_code;
                entry.last_finished_at = Some(Local::now());
                entry.pid = None;
            }
        }

        let persist_status = if succeeded {
            ProcessStatus::Stopped
        } else {
            ProcessStatus::Failed
        };
        if let Err(e) = self
            .update_script_state(name, persist_status, false, exit_code)
            .await
        {
            tracing::error!(script = %name, error = ?e, "Failed to persist state on exit");
        }

        if !self.should_restart(name, &status) {
            self.supervisor.cleanup_process(name);
            return;
        }

        let restart_count = self.supervisor.get_mut(name).map(|p| {
            p.restart_count += 1;
            p.restart_count
        });

        self.supervisor.cleanup_process(name);

        if let Some(count) = restart_count {
            let delay = backoff_delay(count);
            tracing::info!(
                script = %name,
                restart_count = count,
                delay_secs = delay.as_secs(),
                "Scheduling restart after backoff"
            );

            let event_tx = self.event_tx.clone();
            let owned_name = name.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = event_tx
                    .send(DaemonEvent::RestartAfterBackoff {
                        name: owned_name,
                        restart_count: count,
                    })
                    .await;
            });
        }
    }

    async fn handle_restart_after_backoff(&mut self, name: &str, restart_count: u32) {
        if !self.config.has_script(name) {
            tracing::info!(script = %name, "Skipping backoff restart - script removed from config");
            return;
        }

        if self.supervisor.contains(name) {
            tracing::info!(script = %name, "Skipping backoff restart - script already running");
            return;
        }

        let script_def = match self.config.script(name).cloned() {
            Some(def) => def,
            None => return,
        };

        tracing::info!(script = %name, restart_count, "Executing restart after backoff");
        let broadcast_tx = self.register_log_channel(name);
        match self.supervisor.start_script(name, &script_def, broadcast_tx) {
            Ok(ScriptStartResult::Started) => {
                if let Some(proc) = self.supervisor.get_mut(name) {
                    proc.restart_count = restart_count;
                }
                self.update_health_on_start(name).await;
                if let Err(e) = self
                    .update_script_state(name, ProcessStatus::Running, false, None)
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to persist state after backoff restart");
                }
            }
            Ok(ScriptStartResult::AlreadyRunning) => {
                tracing::debug!(script = %name, "Backoff restart - script already running");
            }
            Err(e) => {
                tracing::error!(script = %name, error = ?e, "Failed to restart after backoff");
            }
        }
    }

    async fn handle_cron_tick(&mut self, name: &str) {
        if !self.config.has_script(name) {
            return;
        }

        let script_def = match self.config.script(name).cloned() {
            Some(def) => def,
            None => return,
        };

        let broadcast_tx = self.register_log_channel(name);
        match self.supervisor.start_script(name, &script_def, broadcast_tx) {
            Ok(ScriptStartResult::Started) => {
                tracing::info!(script = %name, "Cron-triggered script started");
                self.update_health_on_start(name).await;
                if let Err(e) = self
                    .update_script_state(name, ProcessStatus::Running, false, None)
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to update state after cron start");
                }
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
        let names: Vec<String> = match name {
            Some(name) => vec![name],
            None => self.config.config()?.scripts.keys().cloned().collect(),
        };

        {
            let mut snapshot = self.health.write().await;
            for n in &names {
                snapshot
                    .entry(n.clone())
                    .or_insert_with(|| ScriptHealth::never_ran(n.clone()));
            }
        }

        for name in names {
            self.start_script(&name).await?;
        }
        Ok(())
    }

    async fn stop_scripts(&mut self, name: Option<String>) -> Result<()> {
        let names: Vec<String> = match name {
            Some(name) => vec![name],
            None => self.supervisor.names(),
        };
        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    async fn start_script(&mut self, name: &str) -> Result<ScriptStartResult> {
        let script_def = self
            .config
            .config()?
            .scripts
            .get(name)
            .ok_or_else(|| Error::ScriptNotFound {
                name: name.to_string(),
            })?
            .clone();

        let cron = script_def.cron.clone();
        let broadcast_tx = self.register_log_channel(name);
        let result = self.supervisor.start_script(name, &script_def, broadcast_tx)?;

        if matches!(result, ScriptStartResult::Started) {
            self.update_health_on_start(name).await;
            self.update_script_state(name, ProcessStatus::Running, false, None)
                .await?;

            if let Some(ref cron_expr) = cron {
                if !self.cron.is_scheduled(name) {
                    self.cron.schedule(name, cron_expr);
                }
            }
        }

        Ok(result)
    }

    async fn stop_script(&mut self, name: &str) -> Result<()> {
        tracing::info!(script = %name, "Stopping script");

        if !self.supervisor.contains(name)
            && !self.state.scripts.iter().any(|s| s.name == name)
        {
            return Err(Error::ScriptNotFound {
                name: name.to_string(),
            });
        }

        if self.supervisor.contains(name) {
            self.update_script_state(name, ProcessStatus::Stopped, true, None)
                .await?;
        }

        self.supervisor.stop_script(name).await?;
        self.log_channels.lock().expect("log_channels mutex poisoned").remove(name);
        self.cron.cancel(name);
        tracing::info!(script = %name, "Script stopped successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.supervisor.shutdown_all().await;
        self.cron.cancel_all();
        self.log_channels.lock().expect("log_channels mutex poisoned").clear();
        Ok(())
    }

    async fn read_logs(&self, name: Option<&str>, tail: u32) -> Result<String> {
        let log_dir = self.supervisor.log_dir();
        log_monitor::ensure_log_dir(log_dir)?;

        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => log_monitor::list_script_names(log_dir),
        };

        if names.is_empty() {
            return Ok("No logs available yet.".to_string());
        }

        let mut output = String::new();
        for (i, script_name) in names.iter().enumerate() {
            let log_path = log_monitor::get_log_path(log_dir, script_name);
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

        {
            let mut snapshot = self.health.write().await;
            for script in &self.state.scripts {
                let state = match script.status {
                    ProcessStatus::Running => ScriptHealthState::Running,
                    ProcessStatus::Failed => ScriptHealthState::Failed,
                    ProcessStatus::Stopped => {
                        if script.exit_code.is_some() {
                            if script.exit_code == Some(0) {
                                ScriptHealthState::Succeeded
                            } else {
                                ScriptHealthState::Failed
                            }
                        } else {
                            ScriptHealthState::Succeeded
                        }
                    }
                };
                let healthy = !matches!(state, ScriptHealthState::Failed);
                snapshot.insert(
                    script.name.clone(),
                    ScriptHealth {
                        name: script.name.clone(),
                        healthy,
                        state,
                        last_exit_code: script.exit_code,
                        last_run_at: script.last_started,
                        last_finished_at: script.last_stopped,
                        pid: None,
                        restart_count: script.restart_count,
                    },
                );
            }
        }

        if let Some(config_path) = self.state.config_path.clone() {
            if config_path.exists() {
                tracing::info!(config = ?config_path, "Loading config from state");
                if let Err(e) = self.config.load(&config_path) {
                    tracing::warn!(error = ?e, "Failed to load config from state, skipping restore");
                    return Ok(());
                }
                self.sync_settings();
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
            if !self.config.has_script(&script.name) {
                tracing::info!(script = %script.name, "Skipping restoration - no longer in config");
                continue;
            }
            tracing::info!(script = %script.name, "Restoring script");
            self.start_script(&script.name).await?;
        }

        let cron_entries: Vec<(String, String)> = self
            .config
            .script_names()
            .into_iter()
            .filter(|name| !self.cron.is_scheduled(name))
            .filter_map(|name| {
                self.config
                    .script(&name)
                    .and_then(|def| def.cron.as_ref().map(|expr| (name, expr.clone())))
            })
            .collect();
        for (name, cron_expr) in cron_entries {
            self.cron.schedule(&name, &cron_expr);
        }

        tracing::info!("State restoration completed");
        Ok(())
    }

    async fn update_health_on_start(&self, name: &str) {
        let pid = self.supervisor.get(name).map(|p| p.pid);
        let restart_count = self
            .supervisor
            .get(name)
            .map(|p| p.restart_count)
            .unwrap_or(0);

        let mut snapshot = self.health.write().await;
        let entry = snapshot
            .entry(name.to_string())
            .or_insert_with(|| ScriptHealth::never_ran(name.to_string()));
        entry.state = ScriptHealthState::Running;
        entry.healthy = true;
        entry.last_run_at = Some(Local::now());
        entry.pid = pid;
        entry.restart_count = restart_count;
    }

    async fn update_script_state(
        &mut self,
        name: &str,
        status: ProcessStatus,
        explicitly_stopped: bool,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let restart_count = self
            .supervisor
            .get(name)
            .map(|p| p.restart_count)
            .unwrap_or(0);
        let start_time = self.supervisor.get(name).and_then(|p| p.start_time);

        let script_state = ScriptState {
            name: name.to_string(),
            status,
            last_started: start_time,
            last_stopped: if matches!(status, ProcessStatus::Stopped | ProcessStatus::Failed) {
                Some(Local::now())
            } else {
                None
            },
            exit_code,
            explicitly_stopped,
            restart_count,
        };

        self.state.update_script(script_state).await?;
        tracing::debug!(script = %name, ?status, "State updated");
        Ok(())
    }

    async fn reload_config(&mut self) -> Result<()> {
        let diff = self.config.reload()?;
        self.sync_settings();

        for name in diff.removed {
            tracing::info!(script = %name, "Script removed from config, stopping");
            if let Err(e) = self.stop_script(&name).await {
                tracing::error!(script = %name, error = ?e, "Failed to stop during reload");
            }
            self.state.remove_script(&name).await?;
        }

        for name in diff.added {
            tracing::info!(script = %name, "New script in config, starting");
            if let Err(e) = self.start_script(&name).await {
                tracing::error!(script = %name, error = ?e, "Failed to start during reload");
            }
        }

        for name in diff.changed {
            tracing::info!(script = %name, "Script config changed, restarting");
            if let Err(e) = self.stop_script(&name).await {
                tracing::error!(script = %name, error = ?e, "Failed to stop during reload");
            }
            self.cron.cancel(&name);
            if let Err(e) = self.start_script(&name).await {
                tracing::error!(script = %name, error = ?e, "Failed to start during reload");
            }
        }

        tracing::info!("Configuration reloaded");
        Ok(())
    }
}
