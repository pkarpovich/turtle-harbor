use crate::daemon::daemon_core::DaemonEvent;
use crate::daemon::scheduler;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub struct CronManager {
    tasks: HashMap<String, JoinHandle<()>>,
    event_tx: mpsc::Sender<DaemonEvent>,
}

impl CronManager {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self {
            tasks: HashMap::new(),
            event_tx,
        }
    }

    pub fn schedule(&mut self, name: &str, cron_expr: &str) {
        self.cancel(name);

        match scheduler::spawn_cron_task(name.to_string(), cron_expr, self.event_tx.clone()) {
            Ok(handle) => {
                self.tasks.insert(name.to_string(), handle);
                tracing::info!(script = %name, "Cron task scheduled");
            }
            Err(e) => {
                tracing::error!(script = %name, error = ?e, "Failed to schedule cron");
            }
        }
    }

    pub fn cancel(&mut self, name: &str) {
        if let Some(handle) = self.tasks.remove(name) {
            handle.abort();
            tracing::debug!(script = %name, "Cron task cancelled");
        }
    }

    pub fn cancel_all(&mut self) {
        for (_, handle) in self.tasks.drain() {
            handle.abort();
        }
    }

    pub fn is_scheduled(&self, name: &str) -> bool {
        self.tasks.contains_key(name)
    }
}
