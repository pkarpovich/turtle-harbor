use crate::common::error::{Error, Result};
use crate::daemon::process_manager::ProcessManager;
use crate::daemon::state::ScriptState;
use chrono::Local;
use cron::Schedule;
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Duration, Instant};

#[derive(Debug)]
pub enum SchedulerMessage {
    ScriptAdded(String),
    ScriptRemoved(String),
    ScriptUpdated(String),
}

pub struct CronScheduler {
    process_manager: Arc<ProcessManager>,
    message_rx: Receiver<SchedulerMessage>,
}

static SCHEDULER_TX: OnceCell<Sender<SchedulerMessage>> = OnceCell::new();

pub fn init_scheduler_tx(tx: Sender<SchedulerMessage>) {
    SCHEDULER_TX
        .set(tx)
        .expect("Scheduler TX already initialized");
}

pub fn get_scheduler_tx() -> Option<&'static Sender<SchedulerMessage>> {
    SCHEDULER_TX.get()
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

    async fn handle_script(&self, state: &ScriptState) -> Result<()> {
        let pm = Arc::clone(&self.process_manager);
        let name = state.name.clone();
        let command = state.command.clone();
        let restart_policy = state.restart_policy.clone();
        let max_restarts = state.max_restarts;
        let cron_expr = state.cron.clone().ok_or_else(|| {
            Error::Process(format!("No cron expression found for script {}", name))
        })?;

        let schedule = Schedule::from_str(&cron_expr).map_err(|e| {
            Error::Process(format!(
                "Invalid cron expression for script {}: {}",
                name, e
            ))
        })?;

        if let Some(next_time) = schedule.upcoming(Local).next() {
            let now = Local::now();
            if next_time > now {
                let duration = next_time
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(Duration::from_secs(0));

                sleep_until(Instant::now() + duration).await;

                match pm
                    .start_script(
                        name.clone(),
                        command,
                        restart_policy,
                        max_restarts,
                        Some(cron_expr.clone()),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            script = %name,
                            next_run = %schedule.upcoming(Local).next().unwrap().format("%Y-%m-%d %H:%M:%S"),
                            "Successfully started scheduled script"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            script = %name,
                            error = ?e,
                            "Failed to start scheduled script"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn start(&mut self) -> Result<()> {
        tracing::info!("Starting cron scheduler");

        let mut script_tasks: HashMap<String, JoinHandle<()>> = HashMap::new();

        loop {
            tokio::select! {
                Some(msg) = self.message_rx.recv() => {
                    tracing::debug!(?msg, "Received scheduler message");
                    match msg {
                        SchedulerMessage::ScriptAdded(name) | SchedulerMessage::ScriptUpdated(name) => {
                            let state = self.process_manager.get_state().await;
                            if let Some(script) = state.iter().find(|s| s.name == name && s.cron.is_some()) {
                                if let Some(handle) = script_tasks.remove(&name) {
                                    handle.abort();
                                }

                                let scheduler = Arc::new(self.clone());
                                let script_state = script.clone();
                                let handle = tokio::spawn(async move {
                                    loop {
                                        if let Err(e) = scheduler.handle_script(&script_state).await {
                                            tracing::error!(
                                                script = %script_state.name,
                                                error = ?e,
                                                "Error in scheduler task"
                                            );
                                            break;
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

impl Clone for CronScheduler {
    fn clone(&self) -> Self {
        let (_, rx) = mpsc::channel(100);
        Self {
            process_manager: Arc::clone(&self.process_manager),
            message_rx: rx,
        }
    }
}
