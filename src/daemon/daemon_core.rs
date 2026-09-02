use crate::common::config::{NatsTrigger, RestartPolicy};
use crate::common::error::{Error, Result};
use crate::common::ipc::{Command, ProcessInfo, ProcessStatus, Response};
use crate::daemon::config_manager::ConfigManager;
use crate::daemon::cron_manager::CronManager;
use crate::daemon::health::{self, HealthSnapshot, ScriptHealth, ScriptHealthState};
use crate::daemon::job::JobOutcome;
use crate::daemon::log_monitor;
use crate::daemon::loki_shipper::{self, LokiLogEntry, LokiShipper};
use crate::daemon::nats_manager::NatsManager;
use crate::daemon::process::ScriptStartResult;
use crate::daemon::process_supervisor::{ProcessSupervisor, StartScript};
use crate::daemon::state::{RunningState, ScriptState};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
        instance_id: u64,
        status: Option<ExitStatus>,
    },
    RestartAfterBackoff {
        name: String,
        restart_count: u32,
    },
    CronTick {
        name: String,
    },
    JobTrigger {
        name: String,
        env: HashMap<OsString, OsString>,
        reply_tx: oneshot::Sender<JobOutcome>,
    },
    JobTimeout {
        name: String,
    },
    ListenerReady {
        name: String,
    },
    ListenerFailed {
        name: String,
        error: String,
    },
    Shutdown,
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_secs: u64 = 15;
    let multiplier = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    let secs = base_secs.saturating_mul(multiplier).min(300);
    Duration::from_secs(secs)
}

pub struct DaemonCore {
    supervisor: ProcessSupervisor,
    config: ConfigManager,
    cron: CronManager,
    nats: NatsManager,
    state: RunningState,
    event_tx: mpsc::Sender<DaemonEvent>,
    event_rx: mpsc::Receiver<DaemonEvent>,
    log_channels: LogChannels,
    health: HealthSnapshot,
    loki_tx: Option<mpsc::Sender<LokiLogEntry>>,
    pending_jobs: HashMap<String, oneshot::Sender<JobOutcome>>,
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
        let nats = NatsManager::new(event_tx.clone());
        let log_channels = Arc::new(Mutex::new(HashMap::new()));
        let health = health::new_health_snapshot();

        Ok(Self {
            supervisor,
            config,
            cron,
            nats,
            state,
            event_tx,
            event_rx,
            log_channels,
            health,
            loki_tx: None,
            pending_jobs: HashMap::new(),
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
                DaemonEvent::ProcessExited {
                    name,
                    instance_id,
                    status,
                } => {
                    self.handle_process_exit(&name, instance_id, status).await;
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
                DaemonEvent::JobTrigger {
                    name,
                    env,
                    reply_tx,
                } => {
                    self.handle_job_trigger(&name, env, reply_tx).await;
                }
                DaemonEvent::JobTimeout { name } => {
                    self.handle_job_timeout(&name).await;
                }
                DaemonEvent::ListenerReady { name } => {
                    self.handle_listener_ready(&name).await;
                }
                DaemonEvent::ListenerFailed { name, error } => {
                    self.handle_listener_failed(&name, &error).await;
                }
                DaemonEvent::Shutdown => {
                    tracing::info!("Shutdown event received");
                    break;
                }
            }
        }

        self.shutdown().await
    }

    fn script_config_path(&self, name: &str) -> Option<PathBuf> {
        self.state
            .scripts
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.config_path.clone())
    }

    fn sync_settings(&mut self, config_path: &Path) {
        if let Some(log_dir) = self.config.log_dir(config_path) {
            let config_dir = self.config.config_dir(config_path);
            let resolved = if log_dir.is_absolute() {
                log_dir.to_path_buf()
            } else {
                config_dir.join(log_dir)
            };
            self.supervisor.set_log_dir(resolved);
        }

        if self.loki_tx.is_none() {
            if let Some(loki_config) = self.config.loki_config(config_path).cloned() {
                let (tx, rx) = mpsc::channel(loki_shipper::CHANNEL_CAPACITY);
                LokiShipper::spawn(rx, loki_config);
                self.supervisor.set_loki_tx(tx.clone());
                self.loki_tx = Some(tx);
            }
        }
    }

    fn full_status_list(&self, filter_config: Option<&Path>) -> Vec<ProcessInfo> {
        let mut result: HashMap<String, ProcessInfo> = HashMap::new();

        for script in &self.state.scripts {
            if let Some(filter) = filter_config {
                if script.config_path.as_deref() != Some(filter) {
                    continue;
                }
            }

            let uptime = match script.status {
                ProcessStatus::Running | ProcessStatus::Restarting => script
                    .last_started
                    .map(|st| {
                        Local::now()
                            .signed_duration_since(st)
                            .to_std()
                            .unwrap_or_default()
                    })
                    .unwrap_or_default(),
                ProcessStatus::Stopped | ProcessStatus::Failed | ProcessStatus::Listening => {
                    Duration::default()
                }
            };

            result.insert(
                script.name.clone(),
                ProcessInfo {
                    name: script.name.clone(),
                    pid: 0,
                    status: script.status,
                    uptime,
                    restart_count: script.restart_count,
                    exit_code: script.exit_code,
                    config_path: script.config_path.clone(),
                },
            );
        }

        for info in self.supervisor.status_list() {
            if let Some(filter) = filter_config {
                let script_config = self.script_config_path(&info.name);
                if script_config.as_deref() != Some(filter) {
                    continue;
                }
            }
            let config_path = self.script_config_path(&info.name);
            result.insert(
                info.name.clone(),
                ProcessInfo {
                    config_path,
                    ..info
                },
            );
        }

        let mut list: Vec<ProcessInfo> = result.into_values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    async fn handle_command(&mut self, command: Command) -> Response {
        match command {
            Command::Up { name, config_path } => {
                if let Err(e) = self.config.load(&config_path) {
                    return Response::Error(e.to_string());
                }
                self.sync_settings(&config_path);

                if let Some(ref script_name) = name {
                    if let Some(existing) = self.config.has_script_globally(script_name) {
                        if existing != config_path {
                            return Response::Error(format!(
                                "script '{}' already registered from '{}'",
                                script_name,
                                existing.display()
                            ));
                        }
                    }
                } else {
                    let new_names: Vec<String> = self.config.script_names(&config_path);
                    for script_name in &new_names {
                        if let Some(existing) = self.config.has_script_globally(script_name) {
                            if existing != config_path {
                                return Response::Error(format!(
                                    "script '{}' already registered from '{}'",
                                    script_name,
                                    existing.display()
                                ));
                            }
                        }
                    }
                }

                match self.start_scripts(name, &config_path).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::Down { name, config_path } => {
                match self.stop_scripts(name, Some(&config_path)).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            }
            Command::Ps { config_path } => {
                Response::ProcessList(self.full_status_list(config_path.as_deref()))
            }
            Command::Logs {
                name,
                tail,
                follow: _,
            } => match self.read_logs(name.as_deref(), tail).await {
                Ok(logs) => Response::Logs(logs),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Reload { config_path } => match self.reload_config(&config_path).await {
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

        let Some(config_path) = self.script_config_path(name) else {
            return false;
        };

        match (
            self.config.script(&config_path, name),
            self.supervisor.get(name),
        ) {
            (Some(def), Some(proc)) => {
                matches!(def.restart_policy, RestartPolicy::Always)
                    && proc.restart_count < def.effective_max_restarts()
            }
            _ => false,
        }
    }

    async fn handle_process_exit(
        &mut self,
        name: &str,
        instance_id: u64,
        status: Option<ExitStatus>,
    ) {
        if self.supervisor.get(name).map(|p| p.instance_id) != Some(instance_id) {
            tracing::debug!(
                script = %name,
                instance_id,
                "Dropping stale exit event - supervisor entry mismatch"
            );
            return;
        }

        let exit_code = status.and_then(|s| s.code());
        let succeeded = status.map(|s| s.success()).unwrap_or(false);

        if let Some(reply_tx) = self.pending_jobs.remove(name) {
            let outcome = match exit_code {
                Some(code) => JobOutcome::Exited(code),
                None => JobOutcome::Signaled,
            };
            tracing::info!(script = %name, ?outcome, "Resolving job with process outcome");
            let _ = reply_tx.send(outcome);
        }

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

        let persist_status = if self.is_job(name) {
            ProcessStatus::Listening
        } else if succeeded {
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

            if let Err(e) = self
                .update_script_state(name, ProcessStatus::Restarting, false, exit_code)
                .await
            {
                tracing::error!(script = %name, error = ?e, "Failed to persist restarting state");
            }

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
        let Some(config_path) = self.script_config_path(name) else {
            tracing::info!(script = %name, "Skipping backoff restart - no config path");
            return;
        };

        if !self.config.has_script(&config_path, name) {
            tracing::info!(script = %name, "Skipping backoff restart - script removed from config");
            return;
        }

        if self.supervisor.contains(name) {
            tracing::info!(script = %name, "Skipping backoff restart - script already running");
            return;
        }

        let was_stopped = self
            .state
            .scripts
            .iter()
            .any(|s| s.name == name && s.explicitly_stopped);
        if was_stopped {
            tracing::info!(script = %name, "Skipping backoff restart - script was explicitly stopped");
            return;
        }

        let script_def = match self.config.script(&config_path, name).cloned() {
            Some(def) => def,
            None => return,
        };

        tracing::info!(script = %name, restart_count, "Executing restart after backoff");
        let config_dir = self.config.config_dir(&config_path);
        let broadcast_tx = self.register_log_channel(name);
        match self.supervisor.start_script(StartScript {
            name,
            script: &script_def,
            broadcast_tx,
            config_dir: &config_dir,
            extra_env: HashMap::new(),
        }) {
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
        let Some(config_path) = self.script_config_path(name) else {
            return;
        };

        if !self.config.has_script(&config_path, name) {
            return;
        }

        let script_def = match self.config.script(&config_path, name).cloned() {
            Some(def) => def,
            None => return,
        };

        let config_dir = self.config.config_dir(&config_path);
        let broadcast_tx = self.register_log_channel(name);
        match self.supervisor.start_script(StartScript {
            name,
            script: &script_def,
            broadcast_tx,
            config_dir: &config_dir,
            extra_env: HashMap::new(),
        }) {
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

    async fn register_listener(&mut self, name: &str, config_path: &Path, trigger: &NatsTrigger) {
        let Some(url) = self.config.nats_url(config_path) else {
            tracing::error!(script = %name, "Cannot register listener - settings.nats.url is missing");
            return;
        };
        if self.nats.is_bound(name, trigger, &url) {
            tracing::debug!(script = %name, "Listener already bound to this trigger - skipping");
            return;
        }
        self.abort_job_before_rebind(name).await;
        self.nats.listen(name, trigger, &url).await;
    }

    fn is_job(&self, name: &str) -> bool {
        let Some(config_path) = self.script_config_path(name) else {
            return false;
        };
        let Some(script_def) = self.config.script(&config_path, name) else {
            return false;
        };
        script_def.nats.is_some()
    }

    async fn handle_job_trigger(
        &mut self,
        name: &str,
        env: HashMap<OsString, OsString>,
        reply_tx: oneshot::Sender<JobOutcome>,
    ) {
        if reply_tx.is_closed() {
            tracing::info!(script = %name, "Job trigger dropped - the listener that sent it is gone");
            return;
        }

        if !self.nats.is_listening(name) {
            tracing::info!(script = %name, "Job trigger declined - listener not registered");
            let _ = reply_tx.send(JobOutcome::NotStarted);
            return;
        }

        let explicitly_stopped = self
            .state
            .scripts
            .iter()
            .any(|s| s.name == name && s.explicitly_stopped);
        if explicitly_stopped {
            tracing::info!(script = %name, "Job trigger declined - script explicitly stopped");
            let _ = reply_tx.send(JobOutcome::NotStarted);
            return;
        }

        let Some(config_path) = self.script_config_path(name) else {
            tracing::info!(script = %name, "Job trigger declined - no config path");
            let _ = reply_tx.send(JobOutcome::NotStarted);
            return;
        };

        if !self.config.has_script(&config_path, name) {
            tracing::info!(script = %name, "Job trigger declined - script removed from config");
            let _ = reply_tx.send(JobOutcome::NotStarted);
            return;
        }

        let Some(script_def) = self.config.script(&config_path, name).cloned() else {
            tracing::info!(script = %name, "Job trigger declined - definition unavailable");
            let _ = reply_tx.send(JobOutcome::NotStarted);
            return;
        };

        let config_dir = self.config.config_dir(&config_path);
        let broadcast_tx = self.register_log_channel(name);
        match self.supervisor.start_script(StartScript {
            name,
            script: &script_def,
            broadcast_tx,
            config_dir: &config_dir,
            extra_env: env,
        }) {
            Ok(ScriptStartResult::Started) => {
                tracing::info!(script = %name, "Job-triggered script started");
                self.pending_jobs.insert(name.to_string(), reply_tx);
                self.update_health_on_start(name).await;
                if let Err(e) = self
                    .update_script_state(name, ProcessStatus::Running, false, None)
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to update state after job start");
                }
            }
            Ok(ScriptStartResult::AlreadyRunning) => {
                tracing::warn!(script = %name, "Job trigger declined - script already running");
                let _ = reply_tx.send(JobOutcome::NotStarted);
            }
            Err(e) => {
                tracing::error!(script = %name, error = ?e, "Job-triggered start failed");
                let _ = reply_tx.send(JobOutcome::NotStarted);
                let mut snapshot = self.health.write().await;
                let entry = snapshot
                    .entry(name.to_string())
                    .or_insert_with(|| ScriptHealth::never_ran(name.to_string()));
                entry.state = ScriptHealthState::Failed;
                entry.healthy = false;
                entry.last_run_at = Some(Local::now());
                entry.last_finished_at = Some(Local::now());
                entry.last_exit_code = None;
                entry.pid = None;
            }
        }
    }

    async fn handle_job_timeout(&mut self, name: &str) {
        let Some(reply_tx) = self.pending_jobs.remove(name) else {
            tracing::debug!(script = %name, "Dropping job timeout - the run already finished");
            return;
        };

        tracing::error!(script = %name, "Job timed out - stopping process");
        let _ = reply_tx.send(JobOutcome::TimedOut);

        {
            let mut snapshot = self.health.write().await;
            if let Some(entry) = snapshot.get_mut(name) {
                entry.state = ScriptHealthState::Failed;
                entry.healthy = false;
                entry.last_finished_at = Some(Local::now());
                entry.last_exit_code = None;
                entry.pid = None;
            }
        }

        if let Err(e) = self
            .update_script_state(name, ProcessStatus::Listening, false, None)
            .await
        {
            tracing::error!(script = %name, error = ?e, "Failed to persist state on job timeout");
        }

        if let Err(e) = self.supervisor.stop_script(name).await {
            tracing::error!(script = %name, error = ?e, "Failed to stop timed-out job");
        }
    }

    async fn abort_job_before_rebind(&mut self, name: &str) {
        if !self.pending_jobs.contains_key(name) {
            return;
        }

        tracing::warn!(script = %name, "Stopping in-flight job before rebinding its listener");

        if let Err(e) = self.supervisor.stop_script(name).await {
            tracing::error!(script = %name, error = ?e, "Failed to stop job during listener rebind");
        }
        self.nats.cancel(name).await;
        self.pending_jobs.remove(name);

        {
            let mut snapshot = self.health.write().await;
            if let Some(entry) = snapshot.get_mut(name) {
                entry.state = ScriptHealthState::Failed;
                entry.healthy = false;
                entry.last_finished_at = Some(Local::now());
                entry.last_exit_code = None;
                entry.pid = None;
            }
        }
    }

    async fn handle_listener_ready(&mut self, name: &str) {
        if !self.nats.is_listening(name) {
            tracing::info!(script = %name, "Listener ready dropped - listener no longer registered");
            return;
        }

        tracing::info!(script = %name, "NATS listener ready");
        let mut snapshot = self.health.write().await;
        let entry = snapshot
            .entry(name.to_string())
            .or_insert_with(|| ScriptHealth::never_ran(name.to_string()));
        if entry.last_run_at.is_none() {
            entry.state = ScriptHealthState::NeverRan;
            entry.healthy = true;
            return;
        }

        let succeeded = entry.last_exit_code == Some(0);
        entry.state = if succeeded {
            ScriptHealthState::Succeeded
        } else {
            ScriptHealthState::Failed
        };
        entry.healthy = succeeded;
    }

    async fn handle_listener_failed(&mut self, name: &str, error: &str) {
        if !self.nats.is_listening(name) {
            tracing::info!(script = %name, "Listener failure dropped - listener no longer registered");
            return;
        }

        tracing::warn!(script = %name, error, "NATS listener failed");
        let mut snapshot = self.health.write().await;
        let entry = snapshot
            .entry(name.to_string())
            .or_insert_with(|| ScriptHealth::never_ran(name.to_string()));
        entry.healthy = false;
        entry.state = ScriptHealthState::Failed;
    }

    async fn start_scripts(&mut self, name: Option<String>, config_path: &Path) -> Result<()> {
        let names: Vec<String> = match name {
            Some(name) => vec![name],
            None => self
                .config
                .config(config_path)?
                .scripts
                .keys()
                .cloned()
                .collect(),
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
            self.start_script(&name, config_path).await?;
        }
        Ok(())
    }

    async fn stop_scripts(
        &mut self,
        name: Option<String>,
        config_path: Option<&Path>,
    ) -> Result<()> {
        let names: Vec<String> = match name {
            Some(name) => vec![name],
            None => {
                let mut seen: HashSet<String> = HashSet::new();
                let mut all: Vec<String> = Vec::new();

                for s in &self.state.scripts {
                    if let Some(filter) = config_path {
                        if s.config_path.as_deref() != Some(filter) {
                            continue;
                        }
                    }
                    if seen.insert(s.name.clone()) {
                        all.push(s.name.clone());
                    }
                }

                for name in self.supervisor.names() {
                    if let Some(filter) = config_path {
                        let script_config = self.script_config_path(&name);
                        if script_config.as_deref() != Some(filter) {
                            continue;
                        }
                    }
                    if seen.insert(name.clone()) {
                        all.push(name);
                    }
                }

                all
            }
        };
        self.nats.signal_stop(&names);
        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    async fn start_script(&mut self, name: &str, config_path: &Path) -> Result<ScriptStartResult> {
        let script_def = self
            .config
            .config(config_path)?
            .scripts
            .get(name)
            .ok_or_else(|| Error::ScriptNotFound {
                name: name.to_string(),
            })?
            .clone();

        if let Some(trigger) = script_def.nats.clone() {
            self.update_script_state_with_config(
                name,
                config_path,
                ProcessStatus::Listening,
                false,
                None,
            )
            .await?;
            {
                let mut snapshot = self.health.write().await;
                snapshot
                    .entry(name.to_string())
                    .or_insert_with(|| ScriptHealth::never_ran(name.to_string()));
            }
            self.register_listener(name, config_path, &trigger).await;
            return Ok(ScriptStartResult::Started);
        }

        let cron = script_def.cron.clone();
        let config_dir = self.config.config_dir(config_path);
        let broadcast_tx = self.register_log_channel(name);
        let result = self.supervisor.start_script(StartScript {
            name,
            script: &script_def,
            broadcast_tx,
            config_dir: &config_dir,
            extra_env: HashMap::new(),
        })?;

        if matches!(result, ScriptStartResult::Started) {
            self.update_health_on_start(name).await;
            self.update_script_state_with_config(
                name,
                config_path,
                ProcessStatus::Running,
                false,
                None,
            )
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

        if !self.supervisor.contains(name) && !self.state.scripts.iter().any(|s| s.name == name) {
            return Err(Error::ScriptNotFound {
                name: name.to_string(),
            });
        }

        self.update_script_state(name, ProcessStatus::Stopped, true, None)
            .await?;

        {
            let mut snapshot = self.health.write().await;
            if let Some(entry) = snapshot.get_mut(name) {
                entry.state = ScriptHealthState::Succeeded;
                entry.healthy = true;
                entry.last_finished_at = Some(Local::now());
                entry.pid = None;
            }
        }

        self.supervisor.stop_script(name).await?;
        self.log_channels
            .lock()
            .expect("log_channels mutex poisoned")
            .remove(name);
        self.cron.cancel(name);
        self.nats.cancel(name).await;
        self.pending_jobs.remove(name);
        tracing::info!(script = %name, "Script stopped successfully");
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.nats.cancel_all().await;
        self.supervisor.shutdown_all().await;
        self.cron.cancel_all();
        self.log_channels
            .lock()
            .expect("log_channels mutex poisoned")
            .clear();
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

        let mut config_paths: Vec<PathBuf> = Vec::new();
        for script in &self.state.scripts {
            if let Some(ref cp) = script.config_path {
                if !config_paths.contains(cp) {
                    config_paths.push(cp.clone());
                }
            }
        }

        let mut parse_failed_paths: HashSet<PathBuf> = HashSet::new();
        for config_path in &config_paths {
            if !config_path.exists() {
                tracing::warn!(config = ?config_path, "Stored config path no longer exists, skipping");
                continue;
            }
            tracing::info!(config = ?config_path, "Loading config from state");
            if let Err(e) = self.config.load(config_path) {
                tracing::warn!(config = ?config_path, error = ?e, "Failed to load config, preserving state for its scripts");
                parse_failed_paths.insert(config_path.clone());
                continue;
            }
            self.sync_settings(config_path);
        }

        let preserve_parse_failure = |script: &ScriptState| -> bool {
            script
                .config_path
                .as_ref()
                .is_some_and(|cp| parse_failed_paths.contains(cp))
        };

        let mut rebound = false;
        for script in self.state.scripts.iter_mut() {
            let Some(global_path) = self.config.has_script_globally(&script.name) else {
                continue;
            };
            if script.config_path.as_ref() != Some(&global_path) {
                tracing::info!(
                    script = %script.name,
                    old = ?script.config_path,
                    new = ?global_path,
                    "Rebinding stored config_path to current location during restore"
                );
                script.config_path = Some(global_path);
                rebound = true;
            }
        }
        if rebound {
            if let Err(e) = self.state.save().await {
                tracing::error!(error = ?e, "Failed to persist rebound config_paths during restore");
            }
        }

        {
            let mut snapshot = self.health.write().await;
            for script in &self.state.scripts {
                if self.config.has_script_globally(&script.name).is_none() {
                    continue;
                }
                let state = match script.status {
                    ProcessStatus::Running | ProcessStatus::Restarting => {
                        ScriptHealthState::Running
                    }
                    ProcessStatus::Failed => ScriptHealthState::Failed,
                    ProcessStatus::Listening => ScriptHealthState::NeverRan,
                    ProcessStatus::Stopped => {
                        if script.exit_code == Some(0) || script.exit_code.is_none() {
                            ScriptHealthState::Succeeded
                        } else {
                            ScriptHealthState::Failed
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

        let orphan_names: Vec<String> = self
            .state
            .scripts
            .iter()
            .filter(|s| !preserve_parse_failure(s) && self.is_orphan(&s.name))
            .map(|s| s.name.clone())
            .collect();
        for name in orphan_names {
            tracing::info!(script = %name, "Pruning orphan from state - not in any loaded config");
            self.forget_script(&name).await;
        }

        let scripts: Vec<ScriptState> = self.state.scripts.clone();
        for script in &scripts {
            if script.explicitly_stopped {
                continue;
            }
            if !matches!(
                script.status,
                ProcessStatus::Running | ProcessStatus::Restarting
            ) {
                continue;
            }
            let Some(ref config_path) = script.config_path else {
                tracing::info!(script = %script.name, "Skipping restoration - no config path");
                continue;
            };
            if !self.config.has_script(config_path, &script.name) {
                tracing::info!(script = %script.name, "Skipping restoration - no longer in config");
                continue;
            }
            tracing::info!(script = %script.name, "Restoring script");
            if let Err(e) = self.start_script(&script.name, config_path).await {
                tracing::error!(script = %script.name, error = ?e, "Failed to restore script");
            }
        }

        for config_path in &config_paths {
            let cron_entries: Vec<(String, String)> = self
                .config
                .script_names(config_path)
                .into_iter()
                .filter(|name| !self.cron.is_scheduled(name))
                .filter(|name| {
                    !scripts
                        .iter()
                        .any(|s| s.name == *name && s.explicitly_stopped)
                })
                .filter_map(|name| {
                    self.config
                        .script(config_path, &name)
                        .and_then(|def| def.cron.as_ref().map(|expr| (name, expr.clone())))
                })
                .collect();

            for (name, cron_expr) in cron_entries {
                self.cron.schedule(&name, &cron_expr);
            }

            let mut listeners: Vec<(String, NatsTrigger, Option<i32>)> = Vec::new();
            for name in self.config.script_names(config_path) {
                if self.nats.is_listening(&name) {
                    continue;
                }
                let mut stored: Option<&ScriptState> = None;
                for script in &scripts {
                    if script.name == name {
                        stored = Some(script);
                    }
                }
                if stored.is_some_and(|script| script.explicitly_stopped) {
                    continue;
                }
                let Some(script_def) = self.config.script(config_path, &name) else {
                    continue;
                };
                let Some(trigger) = script_def.nats.clone() else {
                    continue;
                };
                let exit_code = stored.and_then(|script| script.exit_code);
                listeners.push((name, trigger, exit_code));
            }

            for (name, trigger, exit_code) in listeners {
                tracing::info!(script = %name, "Restoring NATS listener");
                self.register_listener(&name, config_path, &trigger).await;
                if let Err(e) = self
                    .update_script_state_with_config(
                        &name,
                        config_path,
                        ProcessStatus::Listening,
                        false,
                        exit_code,
                    )
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to persist listening state during restore");
                }
            }
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
        let config_path = self.script_config_path(name);
        self.update_script_state_inner(name, config_path, status, explicitly_stopped, exit_code)
            .await
    }

    async fn update_script_state_with_config(
        &mut self,
        name: &str,
        config_path: &Path,
        status: ProcessStatus,
        explicitly_stopped: bool,
        exit_code: Option<i32>,
    ) -> Result<()> {
        self.update_script_state_inner(
            name,
            Some(config_path.to_path_buf()),
            status,
            explicitly_stopped,
            exit_code,
        )
        .await
    }

    async fn update_script_state_inner(
        &mut self,
        name: &str,
        config_path: Option<PathBuf>,
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
            config_path,
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

    fn is_orphan(&self, name: &str) -> bool {
        self.config.has_script_globally(name).is_none()
    }

    async fn forget_script(&mut self, name: &str) {
        if self.supervisor.contains(name) {
            if let Err(e) = self.supervisor.stop_script(name).await {
                tracing::debug!(script = %name, error = ?e, "supervisor.stop during forget_script (ignored)");
            }
            self.log_channels
                .lock()
                .expect("log_channels mutex poisoned")
                .remove(name);
        }
        self.cron.cancel(name);
        self.nats.cancel(name).await;
        self.pending_jobs.remove(name);
        if let Err(e) = self.state.remove_script(name).await {
            tracing::error!(script = %name, error = ?e, "Failed to remove from state during forget_script");
        }
        self.health.write().await.remove(name);
    }

    async fn reload_config(&mut self, config_path: &Path) -> Result<()> {
        let diff = self.config.reload(config_path)?;
        self.sync_settings(config_path);

        self.nats.signal_stop(&diff.removed);
        for name in diff.removed {
            match self.config.has_script_globally(&name) {
                None => {
                    tracing::info!(script = %name, "Script removed from config, pruning state and health");
                    self.forget_script(&name).await;
                }
                Some(other_path) => {
                    tracing::info!(script = %name, surviving = ?other_path, "Script removed from this config but present elsewhere, rebinding");
                    let was_running = self.supervisor.contains(&name);
                    let was_explicitly_stopped = self
                        .state
                        .scripts
                        .iter()
                        .find(|s| s.name == name)
                        .map(|s| s.explicitly_stopped)
                        .unwrap_or(false);
                    if was_running {
                        if let Err(e) = self.supervisor.stop_script(&name).await {
                            tracing::error!(script = %name, error = ?e, "Failed to stop supervisor during rebind");
                        }
                        self.log_channels
                            .lock()
                            .expect("log_channels mutex poisoned")
                            .remove(&name);
                        {
                            let mut snapshot = self.health.write().await;
                            if let Some(entry) = snapshot.get_mut(&name) {
                                entry.state = ScriptHealthState::Succeeded;
                                entry.healthy = true;
                                entry.last_finished_at = Some(Local::now());
                                entry.pid = None;
                            }
                        }
                        if let Err(e) = self
                            .update_script_state_with_config(
                                &name,
                                &other_path,
                                ProcessStatus::Stopped,
                                false,
                                None,
                            )
                            .await
                        {
                            tracing::error!(script = %name, error = ?e, "Failed to persist rebound state");
                        }
                    } else if let Some(entry) =
                        self.state.scripts.iter_mut().find(|s| s.name == name)
                    {
                        entry.config_path = Some(other_path.clone());
                        if let Err(e) = self.state.save().await {
                            tracing::error!(script = %name, error = ?e, "Failed to persist rebound state");
                        }
                    }
                    self.cron.cancel(&name);
                    self.nats.cancel(&name).await;
                    self.pending_jobs.remove(&name);
                    if was_running {
                        if let Err(e) = self.start_script(&name, &other_path).await {
                            tracing::error!(script = %name, error = ?e, "Failed to start from rebound config");
                            {
                                let mut snapshot = self.health.write().await;
                                if let Some(entry) = snapshot.get_mut(&name) {
                                    entry.state = ScriptHealthState::Failed;
                                    entry.healthy = false;
                                    entry.last_finished_at = Some(Local::now());
                                    entry.pid = None;
                                }
                            }
                            if let Err(e) = self
                                .update_script_state_with_config(
                                    &name,
                                    &other_path,
                                    ProcessStatus::Failed,
                                    false,
                                    None,
                                )
                                .await
                            {
                                tracing::error!(script = %name, error = ?e, "Failed to persist failed rebind state");
                            }
                        }
                    } else if !was_explicitly_stopped {
                        let mut trigger: Option<NatsTrigger> = None;
                        if let Some(def) = self.config.script(&other_path, &name) {
                            if let Some(cron_expr) = def.cron.as_ref() {
                                if !self.cron.is_scheduled(&name) {
                                    self.cron.schedule(&name, cron_expr);
                                }
                            }
                            trigger = def.nats.clone();
                        }
                        if let Some(trigger) = trigger {
                            let exit_code = self
                                .state
                                .scripts
                                .iter()
                                .find(|s| s.name == name)
                                .and_then(|s| s.exit_code);
                            self.register_listener(&name, &other_path, &trigger).await;
                            if let Err(e) = self
                                .update_script_state_with_config(
                                    &name,
                                    &other_path,
                                    ProcessStatus::Listening,
                                    false,
                                    exit_code,
                                )
                                .await
                            {
                                tracing::error!(script = %name, error = ?e, "Failed to persist listening state during rebind");
                            }
                        }
                    }
                }
            }
        }

        for name in diff.added {
            tracing::info!(script = %name, "New script in config, starting");
            if let Err(e) = self.start_script(&name, config_path).await {
                tracing::error!(script = %name, error = ?e, "Failed to start during reload");
            }
        }

        self.nats.signal_stop(&diff.changed);
        for name in diff.changed {
            tracing::info!(script = %name, "Script config changed, restarting");
            if let Err(e) = self.stop_script(&name).await {
                tracing::error!(script = %name, error = ?e, "Failed to stop during reload");
            }
            self.cron.cancel(&name);
            self.nats.cancel(&name).await;
            if let Err(e) = self.start_script(&name, config_path).await {
                tracing::error!(script = %name, error = ?e, "Failed to start during reload");
            }
        }

        if diff.nats_url_changed {
            tracing::info!(config = ?config_path, "NATS settings changed, re-registering listeners");
            let mut listeners: Vec<(String, NatsTrigger, Option<i32>)> = Vec::new();
            for name in self.config.script_names(config_path) {
                let mut stored: Option<&ScriptState> = None;
                for script in &self.state.scripts {
                    if script.name == name {
                        stored = Some(script);
                    }
                }
                if stored.is_some_and(|script| script.explicitly_stopped) {
                    continue;
                }
                let Some(script_def) = self.config.script(config_path, &name) else {
                    continue;
                };
                let Some(trigger) = script_def.nats.clone() else {
                    continue;
                };
                let exit_code = stored.and_then(|script| script.exit_code);
                listeners.push((name, trigger, exit_code));
            }

            let rebinding: Vec<String> =
                listeners.iter().map(|(name, _, _)| name.clone()).collect();
            self.nats.signal_stop(&rebinding);
            for (name, trigger, exit_code) in listeners {
                self.abort_job_before_rebind(&name).await;
                self.register_listener(&name, config_path, &trigger).await;
                if let Err(e) = self
                    .update_script_state_with_config(
                        &name,
                        config_path,
                        ProcessStatus::Listening,
                        false,
                        exit_code,
                    )
                    .await
                {
                    tracing::error!(script = %name, error = ?e, "Failed to persist listening state after NATS settings change");
                }
            }
        }

        tracing::info!("Configuration reloaded");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn write_config(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn make_core() -> (DaemonCore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.json");
        let log_dir = tmp.path().join("logs");
        let core = DaemonCore::new(state_file, log_dir).unwrap();
        (core, tmp)
    }

    fn dummy_script_state(name: &str, config_path: Option<PathBuf>) -> ScriptState {
        ScriptState {
            name: name.to_string(),
            config_path,
            status: ProcessStatus::Failed,
            last_started: None,
            last_stopped: None,
            exit_code: Some(1),
            explicitly_stopped: false,
            restart_count: 0,
        }
    }

    const CONFIG_WITH_FOO: &str = r#"
settings:
  log_dir: "./logs"
scripts:
  foo:
    command: "echo foo"
    restart_policy: "never"
"#;

    const CONFIG_WITH_BAR: &str = r#"
settings:
  log_dir: "./logs"
scripts:
  bar:
    command: "echo bar"
    restart_policy: "never"
"#;

    const CONFIG_WITH_BAR_AND_KEEPER: &str = r#"
settings:
  log_dir: "./logs"
scripts:
  bar:
    command: "echo bar"
    restart_policy: "never"
  keeper:
    command: "echo keeper"
    restart_policy: "never"
"#;

    #[tokio::test]
    async fn is_orphan_with_empty_config_returns_true() {
        let (core, _tmp) = make_core();
        assert!(core.is_orphan("anything"));
    }

    #[tokio::test]
    async fn is_orphan_returns_false_when_script_in_loaded_config() {
        let (mut core, _tmp) = make_core();
        let cfg = write_config(CONFIG_WITH_FOO);
        core.config.load(cfg.path()).unwrap();

        assert!(!core.is_orphan("foo"));
        assert!(core.is_orphan("missing"));
    }

    #[tokio::test]
    async fn is_orphan_returns_false_when_script_in_other_loaded_config() {
        let (mut core, _tmp) = make_core();
        let cfg_a = write_config(CONFIG_WITH_FOO);
        let cfg_b = write_config(CONFIG_WITH_BAR);
        core.config.load(cfg_a.path()).unwrap();
        core.config.load(cfg_b.path()).unwrap();

        assert!(!core.is_orphan("foo"));
        assert!(!core.is_orphan("bar"));
        assert!(core.is_orphan("baz"));
    }

    #[tokio::test]
    async fn forget_script_clears_state_and_health() {
        let (mut core, _tmp) = make_core();

        core.state
            .update_script(dummy_script_state("ghost", None))
            .await
            .unwrap();
        core.health.write().await.insert(
            "ghost".to_string(),
            ScriptHealth {
                name: "ghost".to_string(),
                healthy: false,
                state: ScriptHealthState::Failed,
                last_exit_code: Some(1),
                last_run_at: None,
                last_finished_at: None,
                pid: None,
                restart_count: 0,
            },
        );

        core.forget_script("ghost").await;

        assert!(core.state.scripts.iter().all(|s| s.name != "ghost"));
        assert!(!core.health.read().await.contains_key("ghost"));
    }

    #[tokio::test]
    async fn forget_script_is_idempotent_on_unknown_name() {
        let (mut core, _tmp) = make_core();
        core.forget_script("never-existed").await;
        assert!(core.state.scripts.is_empty());
        assert!(core.health.read().await.is_empty());
    }

    #[tokio::test]
    async fn handle_process_exit_drops_event_when_supervisor_empty() {
        let (mut core, _tmp) = make_core();

        core.handle_process_exit("ghost", 42, None).await;

        assert!(
            core.state.scripts.iter().all(|s| s.name != "ghost"),
            "exit event for a name not in supervisor must not touch state"
        );
        assert!(!core.health.read().await.contains_key("ghost"));
    }

    #[tokio::test]
    async fn restore_state_prunes_orphans() {
        let (mut core, _tmp) = make_core();
        let cfg = write_config(CONFIG_WITH_FOO);

        core.state
            .update_script(dummy_script_state("foo", Some(cfg.path().to_path_buf())))
            .await
            .unwrap();
        core.state
            .update_script(dummy_script_state("ghost", Some(cfg.path().to_path_buf())))
            .await
            .unwrap();

        core.restore_state().await.unwrap();

        assert!(core.state.scripts.iter().any(|s| s.name == "foo"));
        assert!(core.state.scripts.iter().all(|s| s.name != "ghost"));
        let health = core.health.read().await;
        assert!(health.contains_key("foo"));
        assert!(!health.contains_key("ghost"));
    }

    #[tokio::test]
    async fn restore_state_keeps_scripts_in_other_config() {
        let (mut core, _tmp) = make_core();
        let cfg_a = write_config(CONFIG_WITH_FOO);
        let cfg_b = write_config(CONFIG_WITH_BAR_AND_KEEPER);

        core.state
            .update_script(dummy_script_state("foo", Some(cfg_a.path().to_path_buf())))
            .await
            .unwrap();
        core.state
            .update_script(dummy_script_state(
                "keeper",
                Some(cfg_b.path().to_path_buf()),
            ))
            .await
            .unwrap();
        core.state
            .update_script(dummy_script_state("bar", Some(cfg_a.path().to_path_buf())))
            .await
            .unwrap();

        core.restore_state().await.unwrap();

        assert!(core.state.scripts.iter().any(|s| s.name == "foo"));
        assert!(core.state.scripts.iter().any(|s| s.name == "keeper"));
        let bar_entry = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "bar")
            .expect("bar should remain in state");
        assert_eq!(
            bar_entry.config_path.as_deref(),
            Some(cfg_b.path()),
            "bar should be rebound to cfg_b after restore",
        );
        let health = core.health.read().await;
        assert!(health.contains_key("foo"));
        assert!(health.contains_key("keeper"));
        assert!(health.contains_key("bar"));
    }

    const EMPTY_CONFIG: &str = r#"
settings:
  log_dir: "./logs"
scripts: {}
"#;

    fn dummy_health(name: &str) -> ScriptHealth {
        ScriptHealth {
            name: name.to_string(),
            healthy: false,
            state: ScriptHealthState::Failed,
            last_exit_code: Some(1),
            last_run_at: None,
            last_finished_at: None,
            pid: None,
            restart_count: 0,
        }
    }

    #[tokio::test]
    async fn reload_removes_from_health() {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("scripts.yml");
        std::fs::write(&cfg_path, CONFIG_WITH_FOO).unwrap();
        core.config.load(&cfg_path).unwrap();

        core.state
            .update_script(dummy_script_state("foo", Some(cfg_path.clone())))
            .await
            .unwrap();
        core.health
            .write()
            .await
            .insert("foo".to_string(), dummy_health("foo"));

        std::fs::write(&cfg_path, EMPTY_CONFIG).unwrap();
        core.reload_config(&cfg_path).await.unwrap();

        assert!(core.state.scripts.iter().all(|s| s.name != "foo"));
        assert!(!core.health.read().await.contains_key("foo"));
    }

    #[tokio::test]
    async fn stop_script_updates_health_to_succeeded() {
        let (mut core, _tmp) = make_core();

        core.state
            .update_script(dummy_script_state("foo", None))
            .await
            .unwrap();
        core.health.write().await.insert(
            "foo".to_string(),
            ScriptHealth {
                name: "foo".to_string(),
                healthy: true,
                state: ScriptHealthState::Running,
                last_exit_code: None,
                last_run_at: Some(Local::now()),
                last_finished_at: None,
                pid: Some(12345),
                restart_count: 0,
            },
        );

        core.stop_script("foo").await.unwrap();

        let snapshot = core.health.read().await;
        let entry = snapshot
            .get("foo")
            .expect("health entry must remain after explicit stop");
        assert!(entry.healthy, "explicit stop should mark script healthy");
        assert!(matches!(entry.state, ScriptHealthState::Succeeded));
        assert_eq!(entry.pid, None, "stale pid must be cleared");
        assert!(entry.last_finished_at.is_some());
    }

    #[tokio::test]
    async fn reload_keeps_script_present_in_other_config() {
        let (mut core, tmp) = make_core();
        let cfg_a_path = tmp.path().join("a.yml");
        let cfg_b_path = tmp.path().join("b.yml");
        std::fs::write(&cfg_a_path, CONFIG_WITH_FOO).unwrap();
        std::fs::write(
            &cfg_b_path,
            r#"
settings:
  log_dir: "./logs"
scripts:
  foo:
    command: "echo foo-from-b"
    restart_policy: "never"
"#,
        )
        .unwrap();
        core.config.load(&cfg_a_path).unwrap();
        core.config.load(&cfg_b_path).unwrap();

        core.state
            .update_script(dummy_script_state("foo", Some(cfg_a_path.clone())))
            .await
            .unwrap();
        core.health
            .write()
            .await
            .insert("foo".to_string(), dummy_health("foo"));

        std::fs::write(&cfg_a_path, EMPTY_CONFIG).unwrap();
        core.reload_config(&cfg_a_path).await.unwrap();

        let foo = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "foo")
            .expect("foo must remain in state");
        assert_eq!(
            foo.config_path.as_deref(),
            Some(cfg_b_path.as_path()),
            "config_path must be rebound to the surviving config"
        );
        assert!(
            !foo.explicitly_stopped,
            "rebinding to a surviving config must not mark the script as explicitly stopped"
        );
        assert!(core.health.read().await.contains_key("foo"));
    }

    #[tokio::test]
    async fn reload_rebind_preserves_explicitly_stopped_and_skips_cron() {
        let (mut core, tmp) = make_core();
        let cfg_a_path = tmp.path().join("a.yml");
        let cfg_b_path = tmp.path().join("b.yml");
        std::fs::write(&cfg_a_path, CONFIG_WITH_FOO).unwrap();
        std::fs::write(
            &cfg_b_path,
            r#"
settings:
  log_dir: "./logs"
scripts:
  foo:
    command: "echo foo-from-b"
    restart_policy: "never"
    cron: "0 */1 * * * * *"
"#,
        )
        .unwrap();
        core.config.load(&cfg_a_path).unwrap();
        core.config.load(&cfg_b_path).unwrap();

        let mut state = dummy_script_state("foo", Some(cfg_a_path.clone()));
        state.explicitly_stopped = true;
        state.status = ProcessStatus::Stopped;
        core.state.update_script(state).await.unwrap();
        core.health
            .write()
            .await
            .insert("foo".to_string(), dummy_health("foo"));

        std::fs::write(&cfg_a_path, EMPTY_CONFIG).unwrap();
        core.reload_config(&cfg_a_path).await.unwrap();

        let foo = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "foo")
            .expect("foo must remain in state");
        assert_eq!(foo.config_path.as_deref(), Some(cfg_b_path.as_path()));
        assert!(
            foo.explicitly_stopped,
            "rebinding a non-running script must not clear explicitly_stopped"
        );
        assert!(
            !core.cron.is_scheduled("foo"),
            "rebinding an explicitly-stopped script must not schedule its cron"
        );
    }

    #[tokio::test]
    async fn restore_state_preserves_entries_when_config_parse_fails() {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("broken.yml");
        std::fs::write(&cfg_path, "scripts: [this is not valid yaml: }}}").unwrap();

        core.state
            .update_script(dummy_script_state("foo", Some(cfg_path.clone())))
            .await
            .unwrap();

        core.restore_state().await.unwrap();

        assert!(
            core.state.scripts.iter().any(|s| s.name == "foo"),
            "scripts referencing a config that exists but failed to parse must not be pruned"
        );
        assert!(
            !core.health.read().await.contains_key("foo"),
            "/health should be a projection of currently-loaded configs; parse-failed entries must be omitted"
        );
    }

    #[tokio::test]
    async fn restore_state_keeps_health_when_script_present_in_other_loaded_config_after_parse_failure(
    ) {
        let (mut core, tmp) = make_core();
        let broken_path = tmp.path().join("broken.yml");
        let working_path = tmp.path().join("working.yml");
        std::fs::write(&broken_path, "scripts: [this is not valid yaml: }}}").unwrap();
        std::fs::write(
            &working_path,
            r#"
settings:
  log_dir: "./logs"
scripts:
  foo:
    command: "echo foo-from-working"
    restart_policy: "never"
  bar:
    command: "echo bar"
    restart_policy: "never"
"#,
        )
        .unwrap();

        core.state
            .update_script(dummy_script_state("foo", Some(broken_path.clone())))
            .await
            .unwrap();
        core.state
            .update_script(dummy_script_state("bar", Some(working_path.clone())))
            .await
            .unwrap();

        core.restore_state().await.unwrap();

        let foo = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "foo")
            .expect("foo must remain in state since it exists in another loaded config");
        assert_eq!(
            foo.config_path.as_deref(),
            Some(working_path.as_path()),
            "foo's config_path must be rebound to the loaded config so it can be restored later"
        );
        let health = core.health.read().await;
        assert!(
            health.contains_key("foo"),
            "foo must appear in health since it exists in another loaded config, despite stored config_path failing to parse"
        );
        assert!(health.contains_key("bar"));
    }

    fn job_config_with_url(command: &str, url: &str) -> String {
        format!(
            r#"
settings:
  log_dir: "./logs"
  nats:
    url: "{url}"
scripts:
  job:
    command: "{command}"
    restart_policy: "never"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "job"
"#
        )
    }

    fn job_config(command: &str) -> String {
        job_config_with_url(command, DEAD_NATS_URL)
    }

    const DEAD_NATS_URL: &str = "nats://127.0.0.1:14222";

    const CONFIG_JOB_WITHOUT_NATS: &str = r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:14222"
scripts:
  job:
    command: "echo job"
    restart_policy: "never"
"#;

    async fn setup_job_core(command: &str) -> (DaemonCore, TempDir, PathBuf) {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("jobs.yml");
        std::fs::write(&cfg_path, job_config(command)).unwrap();
        core.config.load(&cfg_path).unwrap();
        core.state
            .update_script(dummy_script_state("job", Some(cfg_path.clone())))
            .await
            .unwrap();
        let trigger = core
            .config
            .script(&cfg_path, "job")
            .expect("job must be in config")
            .nats
            .clone()
            .expect("job must carry a nats trigger");
        core.register_listener("job", &cfg_path, &trigger).await;
        (core, tmp, cfg_path)
    }

    fn job_env() -> HashMap<OsString, OsString> {
        let mut env = HashMap::new();
        env.insert(OsString::from("TH_JOB_PAYLOAD"), OsString::from("{}"));
        env
    }

    fn is_failed(state: &ScriptHealthState) -> bool {
        match state {
            ScriptHealthState::Failed => true,
            ScriptHealthState::Running => false,
            ScriptHealthState::Succeeded => false,
            ScriptHealthState::NeverRan => false,
        }
    }

    async fn drain_process_exit(core: &mut DaemonCore) {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), core.event_rx.recv())
                .await
                .expect("timed out waiting for ProcessExited")
                .expect("event channel closed");
            let DaemonEvent::ProcessExited {
                name,
                instance_id,
                status,
            } = event
            else {
                continue;
            };
            core.handle_process_exit(&name, instance_id, status).await;
            return;
        }
    }

    #[tokio::test]
    async fn job_trigger_resolves_reply_with_exit_code_and_persists_listening() {
        let (mut core, _tmp, _cfg) = setup_job_core("exit 65").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        drain_process_exit(&mut core).await;

        assert_eq!(reply_rx.await.unwrap(), JobOutcome::Exited(65));

        let entry = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "job")
            .expect("job must remain in state");
        assert_eq!(entry.status, ProcessStatus::Listening);
        assert_eq!(entry.exit_code, Some(65));
        assert!(core.pending_jobs.is_empty());
    }

    #[tokio::test]
    async fn job_trigger_while_running_replies_not_started() {
        let (mut core, _tmp, _cfg) = setup_job_core("sleep 5").await;

        let (first_tx, _first_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), first_tx).await;
        let first_instance = core
            .supervisor
            .get("job")
            .expect("supervisor must hold the first job")
            .instance_id;

        let (second_tx, second_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), second_tx).await;

        assert_eq!(second_rx.await.unwrap(), JobOutcome::NotStarted);
        assert_eq!(
            core.supervisor.get("job").map(|p| p.instance_id),
            Some(first_instance),
            "the running instance must be untouched by the declined trigger"
        );

        core.supervisor.stop_script("job").await.unwrap();
    }

    #[tokio::test]
    async fn job_trigger_for_explicitly_stopped_script_replies_not_started() {
        let (mut core, tmp, cfg_path) = setup_job_core("exit 0").await;

        let mut state = dummy_script_state("job", Some(cfg_path));
        state.status = ProcessStatus::Stopped;
        state.explicitly_stopped = true;
        core.state.update_script(state).await.unwrap();

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;

        assert_eq!(reply_rx.await.unwrap(), JobOutcome::NotStarted);
        assert!(!core.supervisor.contains("job"));
        drop(tmp);
    }

    #[tokio::test]
    async fn job_trigger_for_unknown_script_replies_not_started() {
        let (mut core, _tmp) = make_core();

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("ghost", job_env(), reply_tx).await;

        assert_eq!(reply_rx.await.unwrap(), JobOutcome::NotStarted);
        assert!(!core.supervisor.contains("ghost"));
    }

    #[tokio::test]
    async fn stale_process_exit_leaves_pending_job_untouched() {
        let (mut core, _tmp) = make_core();

        let (reply_tx, reply_rx) = oneshot::channel();
        core.pending_jobs.insert("job".to_string(), reply_tx);

        core.handle_process_exit("job", 999, None).await;

        assert!(core.pending_jobs.contains_key("job"));
        drop(reply_rx);
    }

    #[tokio::test]
    async fn job_timeout_replies_timed_out_and_stops_the_process() {
        let (mut core, _tmp, _cfg) = setup_job_core("sleep 30").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        assert!(core.supervisor.contains("job"));

        core.handle_job_timeout("job").await;

        assert_eq!(reply_rx.await.unwrap(), JobOutcome::TimedOut);
        assert!(!core.supervisor.contains("job"));

        let entry = core
            .state
            .scripts
            .iter()
            .find(|s| s.name == "job")
            .expect("job must remain in state");
        assert_eq!(entry.status, ProcessStatus::Listening);
        assert_eq!(entry.exit_code, None);

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(!health.healthy);
        assert!(is_failed(&health.state));
        assert_eq!(health.pid, None);
    }

    #[tokio::test]
    async fn rebinding_a_listener_stops_the_in_flight_job() {
        let (mut core, _tmp, _cfg) = setup_job_core("sleep 30").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        assert!(core.supervisor.contains("job"));

        core.abort_job_before_rebind("job").await;

        assert!(!core.supervisor.contains("job"));
        assert!(core.pending_jobs.is_empty());
        assert!(!core.nats.is_listening("job"));
        assert!(reply_rx.await.is_err());

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(!health.healthy);
        assert!(is_failed(&health.state));
        assert_eq!(health.pid, None);
    }

    #[tokio::test]
    async fn listener_ready_keeps_a_timed_out_job_unhealthy() {
        let (mut core, _tmp, _cfg) = setup_job_core("exit 0").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        drain_process_exit(&mut core).await;
        assert_eq!(reply_rx.await.unwrap(), JobOutcome::Exited(0));

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        core.handle_job_timeout("job").await;
        assert_eq!(reply_rx.await.unwrap(), JobOutcome::TimedOut);

        core.handle_listener_ready("job").await;

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(
            !health.healthy,
            "a listener rebind must not mask the timed-out run"
        );
        assert!(is_failed(&health.state));
    }

    #[tokio::test]
    async fn job_timeout_after_the_run_finished_is_ignored() {
        let (mut core, _tmp, _cfg) = setup_job_core("exit 0").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        drain_process_exit(&mut core).await;
        assert_eq!(reply_rx.await.unwrap(), JobOutcome::Exited(0));

        core.handle_job_timeout("job").await;

        let entry = stored_status(&core, "job");
        assert_eq!(entry.status, ProcessStatus::Listening);
        assert_eq!(
            entry.exit_code,
            Some(0),
            "a late timeout must not erase the exit code of a finished run"
        );
        assert!(!entry.explicitly_stopped);

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(health.healthy, "a late timeout must not fail a healthy job");
        assert!(!is_failed(&health.state));
    }

    #[tokio::test]
    async fn job_trigger_with_a_gone_listener_does_not_spawn() {
        let (mut core, _tmp, _cfg) = setup_job_core("sleep 30").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        drop(reply_rx);
        core.handle_job_trigger("job", job_env(), reply_tx).await;

        assert!(!core.supervisor.contains("job"));
        assert!(core.pending_jobs.is_empty());
    }

    #[tokio::test]
    async fn stop_script_clears_the_pending_job() {
        let (mut core, _tmp, cfg_path) = setup_job_core("sleep 30").await;

        let (reply_tx, _reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        assert!(core.pending_jobs.contains_key("job"));

        core.stop_scripts(Some("job".to_string()), Some(&cfg_path))
            .await
            .unwrap();

        assert!(core.pending_jobs.is_empty());
        assert!(!core.nats.is_listening("job"));
    }

    #[tokio::test]
    async fn listener_ready_keeps_a_failed_run_unhealthy() {
        let (mut core, _tmp, _cfg) = setup_job_core("exit 1").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        drain_process_exit(&mut core).await;
        assert_eq!(reply_rx.await.unwrap(), JobOutcome::Exited(1));

        core.handle_listener_ready("job").await;

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(
            !health.healthy,
            "a listener rebind must not mask the failed run"
        );
        assert!(is_failed(&health.state));
    }

    #[tokio::test]
    async fn listener_ready_restores_health_after_a_successful_run() {
        let (mut core, _tmp, _cfg) = setup_job_core("exit 0").await;

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        drain_process_exit(&mut core).await;
        assert_eq!(reply_rx.await.unwrap(), JobOutcome::Exited(0));

        core.handle_listener_failed("job", "connection refused")
            .await;
        core.handle_listener_ready("job").await;

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(health.healthy);
        assert!(!is_failed(&health.state));
    }

    #[tokio::test]
    async fn listener_failed_then_ready_toggles_health() {
        let (mut core, _tmp, _cfg) = setup_job_core("echo job").await;

        core.handle_listener_failed("job", "connection refused")
            .await;
        {
            let snapshot = core.health.read().await;
            let health = snapshot.get("job").expect("health entry must exist");
            assert!(!health.healthy);
            assert!(is_failed(&health.state));
            assert_eq!(health.last_exit_code, None);
        }

        core.handle_listener_ready("job").await;
        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(health.healthy);
        assert!(!is_failed(&health.state));
    }

    #[tokio::test]
    async fn listener_failure_queued_behind_a_stop_is_dropped() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;

        core.stop_scripts(Some("job".to_string()), Some(&cfg_path))
            .await
            .unwrap();

        core.handle_listener_failed("job", "connection refused")
            .await;

        let snapshot = core.health.read().await;
        let health = snapshot.get("job").expect("health entry must exist");
        assert!(
            health.healthy,
            "a listener failure sent before the cancel must not outlive the stop"
        );
    }

    #[tokio::test]
    async fn listener_failure_queued_behind_a_forget_creates_no_health_entry() {
        let (mut core, _tmp, _cfg) = setup_listening_core("echo job").await;

        core.forget_script("job").await;

        core.handle_listener_failed("job", "connection refused")
            .await;

        let snapshot = core.health.read().await;
        assert!(
            !snapshot.contains_key("job"),
            "a forgotten script must not be resurrected in /health"
        );
    }

    async fn setup_listening_core(command: &str) -> (DaemonCore, TempDir, PathBuf) {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("jobs.yml");
        std::fs::write(&cfg_path, job_config(command)).unwrap();
        core.config.load(&cfg_path).unwrap();
        core.start_scripts(Some("job".to_string()), &cfg_path)
            .await
            .unwrap();
        (core, tmp, cfg_path)
    }

    fn stored_status(core: &DaemonCore, name: &str) -> ScriptState {
        core.state
            .scripts
            .iter()
            .find(|s| s.name == name)
            .expect("script must be in state")
            .clone()
    }

    #[tokio::test]
    async fn up_on_nats_script_registers_listener_instead_of_spawning() {
        let (core, _tmp, _cfg) = setup_listening_core("echo job").await;

        assert!(core.nats.is_listening("job"));
        assert!(!core.supervisor.contains("job"));

        let entry = stored_status(&core, "job");
        assert_eq!(entry.status, ProcessStatus::Listening);
        assert!(!entry.explicitly_stopped);

        let snapshot = core.health.read().await;
        assert!(snapshot.contains_key("job"));
    }

    #[tokio::test]
    async fn down_on_nats_script_cancels_listener_and_persists_stopped() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;

        core.stop_scripts(Some("job".to_string()), Some(&cfg_path))
            .await
            .unwrap();

        assert!(!core.nats.is_listening("job"));
        let entry = stored_status(&core, "job");
        assert_eq!(entry.status, ProcessStatus::Stopped);
        assert!(entry.explicitly_stopped);
    }

    #[tokio::test]
    async fn job_trigger_without_listener_replies_not_started() {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("jobs.yml");
        std::fs::write(&cfg_path, job_config("echo job")).unwrap();
        core.config.load(&cfg_path).unwrap();
        core.state
            .update_script(dummy_script_state("job", Some(cfg_path)))
            .await
            .unwrap();

        let (reply_tx, reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;

        assert_eq!(reply_rx.await.unwrap(), JobOutcome::NotStarted);
        assert!(!core.supervisor.contains("job"));
    }

    #[tokio::test]
    async fn reload_removing_nats_block_cancels_the_listener() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;

        std::fs::write(&cfg_path, CONFIG_JOB_WITHOUT_NATS).unwrap();
        core.reload_config(&cfg_path).await.unwrap();

        assert!(!core.nats.is_listening("job"));
    }

    #[tokio::test]
    async fn reload_with_a_new_nats_url_persists_the_listening_state() {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("jobs.yml");
        std::fs::write(&cfg_path, job_config("echo job")).unwrap();
        core.config.load(&cfg_path).unwrap();

        std::fs::write(
            &cfg_path,
            job_config_with_url("echo job", "nats://127.0.0.1:14333"),
        )
        .unwrap();
        core.reload_config(&cfg_path).await.unwrap();

        assert!(core.nats.is_listening("job"));
        let entry = stored_status(&core, "job");
        assert_eq!(
            entry.status,
            ProcessStatus::Listening,
            "a listener registered after a URL change must be resolvable by handle_job_trigger"
        );
        assert_eq!(entry.config_path, Some(cfg_path));
    }

    async fn restore_with_stored_job(
        status: ProcessStatus,
        explicitly_stopped: bool,
    ) -> (DaemonCore, TempDir) {
        let (mut core, tmp) = make_core();
        let cfg_path = tmp.path().join("jobs.yml");
        std::fs::write(&cfg_path, job_config("echo job")).unwrap();

        let mut state = dummy_script_state("job", Some(cfg_path));
        state.status = status;
        state.explicitly_stopped = explicitly_stopped;
        state.exit_code = Some(0);
        core.state.update_script(state).await.unwrap();

        core.restore_state().await.unwrap();
        (core, tmp)
    }

    #[tokio::test]
    async fn restore_state_relistens_a_stored_listening_job() {
        let (core, _tmp) = restore_with_stored_job(ProcessStatus::Listening, false).await;

        assert!(core.nats.is_listening("job"));
        assert!(!core.supervisor.contains("job"));
        assert_eq!(stored_status(&core, "job").status, ProcessStatus::Listening);
    }

    #[tokio::test]
    async fn restore_state_relistens_a_stored_stopped_job() {
        let (core, _tmp) = restore_with_stored_job(ProcessStatus::Stopped, false).await;

        assert!(core.nats.is_listening("job"));
        let entry = stored_status(&core, "job");
        assert_eq!(entry.status, ProcessStatus::Listening);
        assert_eq!(entry.exit_code, Some(0));
    }

    #[tokio::test]
    async fn restore_state_skips_an_explicitly_stopped_job() {
        let (core, _tmp) = restore_with_stored_job(ProcessStatus::Stopped, true).await;

        assert!(!core.nats.is_listening("job"));
        assert_eq!(stored_status(&core, "job").status, ProcessStatus::Stopped);
    }

    #[tokio::test]
    async fn reload_with_changed_nats_url_relistens() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;
        core.nats.cancel("job").await;
        assert!(!core.nats.is_listening("job"));

        std::fs::write(
            &cfg_path,
            job_config_with_url("echo job", "nats://127.0.0.1:14223"),
        )
        .unwrap();
        core.reload_config(&cfg_path).await.unwrap();

        assert!(core.nats.is_listening("job"));
    }

    fn trigger_of(core: &DaemonCore, cfg_path: &Path, name: &str) -> NatsTrigger {
        core.config
            .script(cfg_path, name)
            .expect("script must be in config")
            .nats
            .clone()
            .expect("script must carry a nats trigger")
    }

    #[tokio::test]
    async fn up_rebinds_a_listener_after_the_trigger_changes() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;
        let old = trigger_of(&core, &cfg_path, "job");
        assert!(core.nats.is_bound("job", &old, DEAD_NATS_URL));

        std::fs::write(
            &cfg_path,
            job_config("echo job").replace("recordings.completed", "recordings.retried"),
        )
        .unwrap();
        core.config.load(&cfg_path).unwrap();
        core.start_scripts(Some("job".to_string()), &cfg_path)
            .await
            .unwrap();

        let new = trigger_of(&core, &cfg_path, "job");
        assert_ne!(old.subject, new.subject);
        assert!(
            core.nats.is_bound("job", &new, DEAD_NATS_URL),
            "up must rebind the listener to the subject the config now names"
        );
        assert!(!core.nats.is_bound("job", &old, DEAD_NATS_URL));
    }

    #[tokio::test]
    async fn up_rebinds_a_listener_after_the_nats_url_changes() {
        let (mut core, _tmp, cfg_path) = setup_listening_core("echo job").await;
        let trigger = trigger_of(&core, &cfg_path, "job");
        assert!(core.nats.is_bound("job", &trigger, DEAD_NATS_URL));

        let moved = "nats://127.0.0.1:14444";
        std::fs::write(&cfg_path, job_config_with_url("echo job", moved)).unwrap();
        core.config.load(&cfg_path).unwrap();
        core.start_scripts(Some("job".to_string()), &cfg_path)
            .await
            .unwrap();

        assert!(core.nats.is_bound("job", &trigger, moved));
        assert!(!core.nats.is_bound("job", &trigger, DEAD_NATS_URL));
    }

    #[tokio::test]
    async fn shutdown_cancels_listeners_and_stops_the_running_job() {
        let (mut core, _tmp, _cfg) = setup_job_core("sleep 30").await;

        let (reply_tx, _reply_rx) = oneshot::channel();
        core.handle_job_trigger("job", job_env(), reply_tx).await;
        assert!(core.nats.is_listening("job"));
        assert!(core.supervisor.contains("job"));

        core.shutdown().await.unwrap();

        assert!(!core.nats.is_listening("job"));
        assert!(core.supervisor.names().is_empty());
    }

    #[tokio::test]
    async fn restore_state_drops_entries_for_missing_config_file() {
        let (mut core, tmp) = make_core();
        let missing_config = tmp.path().join("nonexistent-config.yml");

        core.state
            .update_script(dummy_script_state("ghost", Some(missing_config)))
            .await
            .unwrap();

        core.restore_state().await.unwrap();

        assert!(core.state.scripts.is_empty());
        let health = core.health.read().await;
        assert!(!health.contains_key("ghost"));
    }
}
