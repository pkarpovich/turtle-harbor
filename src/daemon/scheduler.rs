use crate::common::error::{Result};
use crate::daemon::process::ProcessManager;
use chrono::{Local};
use cron::Schedule;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{sleep_until, Duration, Instant};

pub struct CronScheduler {
    process_manager: Arc<ProcessManager>,
}

impl CronScheduler {
    pub fn new(process_manager: Arc<ProcessManager>) -> Self {
        Self { process_manager }
    }

    pub async fn start(&self) -> Result<()> {
        tracing::info!("Starting cron scheduler");

        let scheduled_scripts = {
            let state = self.process_manager.get_state().await;
            state.into_iter()
                .filter(|script| script.cron.is_some())
                .collect::<Vec<_>>()
        };

        for script in scheduled_scripts {
            let pm = Arc::clone(&self.process_manager);
            let name = script.name.clone();
            let command = script.command.clone();
            let restart_policy = script.restart_policy.clone();
            let max_restarts = script.max_restarts;
            let cron_expr = script.cron.unwrap();

            tokio::spawn(async move {
                let schedule = match Schedule::from_str(&cron_expr) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(
                            script = %name,
                            error = %e,
                            "Invalid cron expression"
                        );
                        return;
                    }
                };

                loop {
                    if let Some(next_time) = schedule.upcoming(Local).next() {
                        let now = Local::now();
                        if next_time > now {
                            let duration = next_time
                                .signed_duration_since(now)
                                .to_std()
                                .unwrap_or(Duration::from_secs(0));

                            sleep_until(Instant::now() + duration).await;

                            match pm.start_script(
                                name.clone(),
                                command.clone(),
                                restart_policy.clone(),
                                max_restarts,
                                Some(cron_expr.clone()),
                            ).await {
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
                    } else {
                        tracing::error!(
                            script = %name,
                            "Could not determine next schedule time"
                        );
                        break;
                    }
                }
            });
        }

        Ok(())
    }
}