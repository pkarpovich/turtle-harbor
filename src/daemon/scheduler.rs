use crate::common::error::{Error, Result};
use crate::daemon::process_manager::ProcessManager;
use crate::daemon::state::ScriptState;
use chrono::Local;
use cron::Schedule;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Duration, Instant};

#[derive(Debug)]
pub enum SchedulerMessage {
    ScriptRemoved(String),
    ScriptUpdated(String),
}

pub struct CronScheduler {
    process_manager: Arc<ProcessManager>,
    message_rx: Receiver<SchedulerMessage>,
}

static SCHEDULER_TX: OnceLock<Sender<SchedulerMessage>> = OnceLock::new();

pub fn init_scheduler_tx(tx: Sender<SchedulerMessage>) {
    SCHEDULER_TX
        .set(tx)
        .expect("Scheduler TX already initialized");
}

pub fn get_scheduler_tx() -> Option<&'static Sender<SchedulerMessage>> {
    SCHEDULER_TX.get()
}

async fn run_scheduled_script(pm: &ProcessManager, state: &ScriptState) -> Result<()> {
    let cron_expr = state.cron.as_ref().ok_or_else(|| {
        Error::Process(format!("No cron expression found for script {}", state.name))
    })?;

    let schedule = Schedule::from_str(cron_expr).map_err(|e| {
        Error::Process(format!(
            "Invalid cron expression for script {}: {}",
            state.name, e
        ))
    })?;

    let next_time = match schedule.upcoming(Local).next() {
        Some(t) => t,
        None => return Ok(()),
    };

    let now = Local::now();
    if next_time <= now {
        return Ok(());
    }

    let duration = next_time
        .signed_duration_since(now)
        .to_std()
        .unwrap_or(Duration::from_secs(0));

    sleep_until(Instant::now() + duration).await;

    match pm
        .start_script(
            state.name.clone(),
            state.command.clone(),
            state.restart_policy.clone(),
            state.max_restarts,
            state.cron.clone(),
        )
        .await
    {
        Ok(_) => {
            tracing::info!(
                script = %state.name,
                next_run = %schedule.upcoming(Local).next().unwrap().format("%Y-%m-%d %H:%M:%S"),
                "Successfully started scheduled script"
            );
        }
        Err(e) => {
            tracing::error!(
                script = %state.name,
                error = ?e,
                "Failed to start scheduled script"
            );
        }
    }

    Ok(())
}

impl CronScheduler {
    pub fn new(process_manager: Arc<ProcessManager>) -> (Self, Sender<SchedulerMessage>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                process_manager,
                message_rx: rx,
            },
            tx,
        )
    }

    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting cron scheduler");

        let mut script_tasks: HashMap<String, JoinHandle<()>> = HashMap::new();

        loop {
            tokio::select! {
                Some(msg) = self.message_rx.recv() => {
                    tracing::debug!(?msg, "Received scheduler message");
                    match msg {
                        SchedulerMessage::ScriptUpdated(name) => {
                            let state = self.process_manager.get_state().await;
                            if let Some(script) = state.iter().find(|s| s.name == name && s.cron.is_some()) {
                                if let Some(handle) = script_tasks.remove(&name) {
                                    handle.abort();
                                }

                                let pm = Arc::clone(&self.process_manager);
                                let script_state = script.clone();
                                let handle = tokio::spawn(async move {
                                    let mut consecutive_failures: u32 = 0;
                                    loop {
                                        match run_scheduled_script(&pm, &script_state).await {
                                            Ok(_) => {
                                                consecutive_failures = 0;
                                            }
                                            Err(e) => {
                                                consecutive_failures += 1;
                                                tracing::error!(
                                                    script = %script_state.name,
                                                    error = ?e,
                                                    consecutive_failures,
                                                    "Scheduler task error, will retry"
                                                );
                                                let backoff_secs = 2u64.saturating_pow(consecutive_failures.min(6));
                                                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                                            }
                                        }
                                    }
                                });
                                script_tasks.insert(name.clone(), handle);
                            }
                        }
                        SchedulerMessage::ScriptRemoved(name) => {
                            if let Some(handle) = script_tasks.remove(&name) {
                                handle.abort();
                                tracing::info!(script = %name, "Removed scheduler task");
                            }
                        }
                    }
                }
            }
        }
    }
}

