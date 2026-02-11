use crate::common::config::Script;
use crate::common::error::Result;
use crate::common::ipc::{ProcessInfo, ProcessStatus};
use crate::daemon::daemon_core::DaemonEvent;
use crate::daemon::log_monitor::{self, ScriptLogger};
use crate::daemon::process::{is_process_alive, ManagedProcess, ScriptStartResult};
use chrono::Local;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::Command as TokioCommand;
use tokio::sync::{broadcast, mpsc};

pub struct ProcessSupervisor {
    processes: HashMap<String, ManagedProcess>,
    event_tx: mpsc::Sender<DaemonEvent>,
    log_dir: PathBuf,
}

impl ProcessSupervisor {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>, log_dir: PathBuf) -> Self {
        Self {
            processes: HashMap::new(),
            event_tx,
            log_dir,
        }
    }

    pub fn set_log_dir(&mut self, log_dir: PathBuf) {
        self.log_dir = log_dir;
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn start_script(&mut self, name: &str, script_def: &Script, broadcast_tx: broadcast::Sender<String>) -> Result<ScriptStartResult> {
        tracing::info!(
            script = %name,
            command = %script_def.command,
            "Starting script"
        );

        if let Some(process) = self.processes.get(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                tracing::info!(script = %name, "Script is still running - skipping");
                return Ok(ScriptStartResult::AlreadyRunning);
            }
        }
        self.cleanup_process(name);

        let canonical_command = std::fs::canonicalize(&script_def.command)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| script_def.command.clone());

        let log_path = log_monitor::get_log_path(&self.log_dir, name);
        log_monitor::ensure_log_dir(&self.log_dir)?;
        let logger = ScriptLogger::new(log_path, broadcast_tx)?;

        let mut child = unsafe {
            TokioCommand::new("sh")
                .arg("-c")
                .arg(&canonical_command)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()?
        };

        let pid = child.id().unwrap_or(0);

        if let Some(stdout) = child.stdout.take() {
            log_monitor::spawn_stdout_reader(BufReader::new(stdout), logger.tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            log_monitor::spawn_stderr_reader(BufReader::new(stderr), logger.tx.clone());
        }

        let event_tx = self.event_tx.clone();
        let watcher_name = name.to_string();
        let watcher = tokio::spawn(async move {
            let status = child.wait().await;
            let _ = event_tx
                .send(DaemonEvent::ProcessExited {
                    name: watcher_name,
                    status: status.ok(),
                })
                .await;
        });

        self.processes.insert(
            name.to_string(),
            ManagedProcess {
                pid,
                status: ProcessStatus::Running,
                start_time: Some(Local::now()),
                restart_count: 0,
                logger: Some(logger),
                watcher: Some(watcher),
            },
        );

        tracing::info!(script = %name, pid, "Script started successfully");
        Ok(ScriptStartResult::Started)
    }

    pub async fn stop_script(&mut self, name: &str) -> Result<()> {
        if let Some(mut process) = self.processes.remove(name) {
            if process.pid > 0 && is_process_alive(process.pid) {
                let pgid = process.pid as i32;
                unsafe { libc::killpg(pgid, libc::SIGTERM) };

                if let Some(watcher) = process.watcher.take() {
                    match tokio::time::timeout(Duration::from_secs(5), watcher).await {
                        Ok(_) => tracing::debug!(script = %name, "Process group exited after SIGTERM"),
                        Err(_) => {
                            tracing::warn!(script = %name, "SIGTERM timeout, sending SIGKILL to process group");
                            unsafe { libc::killpg(pgid, libc::SIGKILL) };
                        }
                    }
                }
            } else if let Some(watcher) = process.watcher.take() {
                watcher.abort();
            }

            if let Some(logger) = process.logger.take() {
                logger.shutdown();
            }
        }
        Ok(())
    }

    pub fn cleanup_process(&mut self, name: &str) {
        if let Some(mut process) = self.processes.remove(name) {
            if let Some(watcher) = process.watcher.take() {
                watcher.abort();
            }
            if let Some(logger) = process.logger.take() {
                logger.shutdown();
            }
        }
    }

    pub async fn shutdown_all(&mut self) {
        tracing::info!("Shutting down all processes");

        let names: Vec<String> = self.processes.keys().cloned().collect();
        for name in names {
            if let Some(mut process) = self.processes.remove(&name) {
                tracing::debug!(script = %name, "Killing process");

                if process.pid > 0 && is_process_alive(process.pid) {
                    let pgid = process.pid as i32;
                    unsafe { libc::killpg(pgid, libc::SIGTERM) };
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    if is_process_alive(process.pid) {
                        unsafe { libc::killpg(pgid, libc::SIGKILL) };
                    }
                }

                if let Some(watcher) = process.watcher.take() {
                    watcher.abort();
                }
                if let Some(logger) = process.logger.take() {
                    logger.shutdown();
                }
            }
        }

        tracing::info!("All processes shut down");
    }

    pub fn status_list(&self) -> Vec<ProcessInfo> {
        self.processes
            .iter()
            .map(|(name, process)| {
                let uptime = process
                    .start_time
                    .map(|st| {
                        Local::now()
                            .signed_duration_since(st)
                            .to_std()
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                ProcessInfo {
                    name: name.clone(),
                    pid: process.pid,
                    status: process.status.clone(),
                    uptime,
                    restart_count: process.restart_count,
                }
            })
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.processes.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&ManagedProcess> {
        self.processes.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut ManagedProcess> {
        self.processes.get_mut(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.processes.keys().cloned().collect()
    }
}
