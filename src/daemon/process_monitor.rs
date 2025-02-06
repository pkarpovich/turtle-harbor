use crate::common::config::RestartPolicy;
use crate::common::error::Result;
use crate::daemon::process::ManagedProcess;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

pub async fn should_restart(
    processes: &Arc<Mutex<HashMap<String, ManagedProcess>>>,
    name: &str,
) -> Option<(String, RestartPolicy, u32)> {
    let mut processes_guard = processes.lock().await;
    if let Some(process) = processes_guard.get_mut(name) {
        if let Ok(Some(_)) = process.child.try_wait() {
            if matches!(process.restart_policy, RestartPolicy::Always)
                && process.restart_count < process.max_restarts
            {
                process.restart_count += 1;
                tracing::info!(
                    script = %name,
                    restart_count = process.restart_count,
                    max_restarts = process.max_restarts,
                    "Restarting process"
                );
                return Some((
                    process.command.clone(),
                    process.restart_policy.clone(),
                    process.max_restarts,
                ));
            } else {
                tracing::debug!(script = %name, "Process will not be restarted");
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
    F: Fn(String, String, RestartPolicy, u32) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send,
{
    if let Some((command, policy, max_restarts)) = should_restart(&processes, name).await {
        if let Err(e) = restart_handler(name.to_string(), command, policy, max_restarts).await {
            tracing::error!(script = %name, error = ?e, "Failed to restart script");
        }
    }
}

pub async fn monitor_and_restart<F, Fut>(
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    restart_handler: Arc<F>,
) where
    F: Fn(String, String, RestartPolicy, u32) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send,
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
