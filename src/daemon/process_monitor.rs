use crate::common::config::RestartPolicy;
use crate::common::error::Result;
use crate::daemon::process::{ManagedProcess, ScriptStartResult};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

#[derive(Debug, Clone, Copy)]
pub enum ProcessExitStatus {
    Success,
    Error(i32),
    Terminated,
}

pub async fn should_restart(
    processes: &Arc<Mutex<HashMap<String, ManagedProcess>>>,
    name: &str,
) -> Option<(String, RestartPolicy, u32, ProcessExitStatus, Option<String>)> {
    let mut processes_guard = processes.lock().await;
    if let Some(process) = processes_guard.get_mut(name) {
        if let Ok(Some(status)) = process.child.try_wait() {
            let exit_status = if status.success() {
                ProcessExitStatus::Success
            } else if let Some(code) = status.code() {
                ProcessExitStatus::Error(code)
            } else {
                ProcessExitStatus::Terminated
            };

            match (exit_status, &process.restart_policy) {
                (ProcessExitStatus::Error(_), RestartPolicy::Always)
                    if process.restart_count < process.max_restarts => {
                        process.restart_count += 1;
                        tracing::info!(
                        script = %name,
                        restart_count = process.restart_count,
                        max_restarts = process.max_restarts,
                        "Process failed - initiating restart"
                    );
                        return Some((
                            process.command.clone(),
                            process.restart_policy.clone(),
                            process.max_restarts,
                            exit_status.clone(),
                            process.cron.clone(),
                        ));
                }
                (ProcessExitStatus::Success, _) => {}
                (ProcessExitStatus::Error(code), _) => {
                    tracing::warn!(
                        script = %name,
                        exit_code = code,
                        restart_count = process.restart_count,
                        max_restarts = process.max_restarts,
                        "Process failed - max restarts reached or restart policy is Never"
                    );
                }
                (ProcessExitStatus::Terminated, _) => {
                    tracing::info!(script = %name, "Process was terminated - no restart needed");
                }
            }
        }
    }
    None
}

pub async fn check_and_restart_if_needed<F, Fut>(
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    name: &str,
    restart_handler: Arc<F>,
) where
    F: Fn(String, String, RestartPolicy, u32, ProcessExitStatus, Option<String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ScriptStartResult>> + Send,
{
    if let Some((command, policy, max_restarts, exit_status, cron)) = should_restart(&processes, name).await {
        match exit_status {
            ProcessExitStatus::Error(code) => {
                tracing::warn!(
                    script = %name,
                    exit_code = code,
                    "Attempting to restart failed process"
                );
                if let Err(e) = restart_handler(name.to_string(), command, policy, max_restarts, exit_status, cron).await {
                    tracing::error!(
                        script = %name,
                        error = ?e,
                        "Failed to restart script after error"
                    );
                }
            }
            _ => {
                tracing::debug!(
                    script = %name,
                    status = ?exit_status,
                    "Process status change - no restart needed"
                );
            }
        }
    }
}

pub async fn monitor_and_restart<F, Fut>(
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    restart_handler: Arc<F>,
) where
    F: Fn(String, String, RestartPolicy, u32, ProcessExitStatus, Option<String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ScriptStartResult>> + Send,
{
    tracing::info!("Starting process monitor");
    loop {
        let names = {
            let processes_guard = processes.lock().await;
            processes_guard.keys().cloned().collect::<Vec<_>>()
        };

        for name in names {
            check_and_restart_if_needed(processes.clone(), &name, Arc::clone(&restart_handler))
                .await;
        }

        sleep(Duration::from_secs(1)).await;
    }
}
