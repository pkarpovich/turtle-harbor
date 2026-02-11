use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{Command, ProcessStatus, Response};
use crate::daemon::config_manager::ConfigManager;
use crate::daemon::cron_manager::CronManager;
use crate::daemon::log_monitor;
use crate::daemon::process::ScriptStartResult;
use crate::daemon::process_supervisor::ProcessSupervisor;
use crate::daemon::state::{RunningState, ScriptState};
use chrono::Local;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
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
    CronTick {
        name: String,
    },
    Shutdown,
}

pub struct DaemonCore {
    supervisor: ProcessSupervisor,
    config: ConfigManager,
    cron: CronManager,
    state: RunningState,
    event_tx: mpsc::Sender<DaemonEvent>,
    event_rx: mpsc::Receiver<DaemonEvent>,
    log_channels: LogChannels,
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

        Ok(Self {
            supervisor,
            config,
            cron,
            state,
            event_tx,
            event_rx,
            log_channels,
        })
    }

    pub fn event_tx(&self) -> mpsc::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    pub fn log_channels(&self) -> LogChannels {
        self.log_channels.clone()
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

    fn sync_log_dir(&mut self) {
        if let Some(log_dir) = self.config.log_dir() {
            self.supervisor.set_log_dir(log_dir.to_path_buf());
        }
    }

    async fn handle_command(&mut self, command: Command) -> Response {
        match command {
            Command::Up { name, config_path } => {
                if let Err(e) = self.config.load(&config_path) {
                    return Response::Error(e.to_string());
                }
                self.sync_log_dir();
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

    async fn handle_process_exit(&mut self, name: &str, status: Option<ExitStatus>) {
        let script_def = self.config.script(name);
        let should_restart = match &status {
            Some(s) if s.success() => {
                tracing::info!(script = %name, "Process exited successfully");
                false
            }
            Some(s) => {
                let code = s.code().unwrap_or(-1);
                tracing::warn!(script = %name, exit_code = code, "Process exited with error");
                match (script_def, self.supervisor.get(name)) {
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

        if !should_restart {
            self.supervisor.cleanup_process(name);
            return;
        }

        let restart_count = self.supervisor.get_mut(name).map(|p| {
            p.restart_count += 1;
            tracing::info!(
                script = %name,
                restart_count = p.restart_count,
                "Initiating restart"
            );
            p.restart_count
        });

        self.supervisor.cleanup_process(name);

        if let Some(count) = restart_count {
            if let Some(script_def) = self.config.script(name).cloned() {
                let (broadcast_tx, _) = broadcast::channel(256);
                self.log_channels
                    .lock()
                    .unwrap()
                    .insert(name.to_string(), broadcast_tx.clone());
                if let Err(e) = self.supervisor.start_script(name, &script_def, broadcast_tx) {
                    tracing::error!(script = %name, error = ?e, "Failed to restart");
                } else if let Some(proc) = self.supervisor.get_mut(name) {
                    proc.restart_count = count;
                }
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

        let (broadcast_tx, _) = broadcast::channel(256);
        self.log_channels
            .lock()
            .unwrap()
            .insert(name.to_string(), broadcast_tx.clone());
        match self.supervisor.start_script(name, &script_def, broadcast_tx) {
            Ok(ScriptStartResult::Started) => {
                tracing::info!(script = %name, "Cron-triggered script started");
                let _ = self
                    .update_script_state(name, ProcessStatus::Running, false)
                    .await;
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
        let (broadcast_tx, _) = broadcast::channel(256);
        self.log_channels
            .lock()
            .unwrap()
            .insert(name.to_string(), broadcast_tx.clone());
        let result = self.supervisor.start_script(name, &script_def, broadcast_tx)?;

        if matches!(result, ScriptStartResult::Started) {
            self.update_script_state(name, ProcessStatus::Running, false)
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
            self.update_script_state(name, ProcessStatus::Stopped, true)
                .await?;
        }

        self.supervisor.stop_script(name).await?;
        self.log_channels.lock().unwrap().remove(name);
        self.cron.cancel(name);
        tracing::info!(script = %name, "Script stopped successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.supervisor.shutdown_all().await;
        self.cron.cancel_all();
        self.log_channels.lock().unwrap().clear();
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

        if let Some(config_path) = self.state.config_path.clone() {
            if config_path.exists() {
                tracing::info!(config = ?config_path, "Loading config from state");
                if let Err(e) = self.config.load(&config_path) {
                    tracing::warn!(error = ?e, "Failed to load config from state, skipping restore");
                    return Ok(());
                }
                self.sync_log_dir();
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

    async fn update_script_state(
        &mut self,
        name: &str,
        status: ProcessStatus,
        explicitly_stopped: bool,
    ) -> Result<()> {
        let restart_count = self
            .supervisor
            .get(name)
            .map(|p| p.restart_count)
            .unwrap_or(0);
        let start_time = self.supervisor.get(name).and_then(|p| p.start_time);

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
        let diff = self.config.reload()?;
        self.sync_log_dir();

        for name in diff.removed {
            tracing::info!(script = %name, "Script removed from config, stopping");
            let _ = self.stop_script(&name).await;
            self.state.remove_script(&name).await?;
        }

        for name in diff.added {
            tracing::info!(script = %name, "New script in config, starting");
            let _ = self.start_script(&name).await;
        }

        for name in diff.changed {
            tracing::info!(script = %name, "Script config changed, restarting");
            let _ = self.stop_script(&name).await;
            self.cron.cancel(&name);
            let _ = self.start_script(&name).await;
        }

        tracing::info!("Configuration reloaded");
        Ok(())
    }
}
