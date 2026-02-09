use crate::common::config::RestartPolicy;
use crate::daemon::process_manager::ProcessManager;
use std::process::ExitStatus;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ProcessExitEvent {
    pub name: String,
    pub status: Option<ExitStatus>,
}

pub async fn monitor_exits(
    mut exit_rx: mpsc::Receiver<ProcessExitEvent>,
    process_manager: Arc<ProcessManager>,
) {
    tracing::info!("Process monitor started (event-driven)");

    while let Some(event) = exit_rx.recv().await {
        let exit_status = match &event.status {
            Some(s) if s.success() => {
                tracing::info!(script = %event.name, "Process exited successfully");
                continue;
            }
            Some(s) => {
                let code = s.code().unwrap_or(-1);
                tracing::warn!(script = %event.name, exit_code = code, "Process exited with error");
                code
            }
            None => {
                tracing::info!(script = %event.name, "Process terminated by signal");
                continue;
            }
        };

        let restart_info = {
            let mut processes = process_manager.processes.lock().await;
            if let Some(process) = processes.get_mut(&event.name) {
                match &process.restart_policy {
                    RestartPolicy::Always if process.restart_count < process.max_restarts => {
                        process.restart_count += 1;
                        tracing::info!(
                            script = %event.name,
                            exit_code = exit_status,
                            restart_count = process.restart_count,
                            max_restarts = process.max_restarts,
                            "Initiating restart"
                        );
                        Some((
                            process.command.clone(),
                            process.restart_policy.clone(),
                            process.max_restarts,
                            process.cron.clone(),
                        ))
                    }
                    _ => {
                        tracing::warn!(
                            script = %event.name,
                            exit_code = exit_status,
                            restart_count = process.restart_count,
                            max_restarts = process.max_restarts,
                            "Max restarts reached or policy is Never"
                        );
                        processes.remove(&event.name);
                        None
                    }
                }
            } else {
                None
            }
        };

        if let Some((command, policy, max_restarts, cron)) = restart_info {
            {
                process_manager.processes.lock().await.remove(&event.name);
            }
            if let Err(e) = process_manager
                .start_script(event.name.clone(), command, policy, max_restarts, cron)
                .await
            {
                tracing::error!(
                    script = %event.name,
                    error = ?e,
                    "Failed to restart script"
                );
            }
        }
    }

    tracing::info!("Process monitor stopped");
}
