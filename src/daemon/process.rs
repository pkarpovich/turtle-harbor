use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep};

use crate::common::config::{RestartPolicy};
use crate::common::error::Error;
use crate::common::ipc::{ProcessStatus, ProcessInfo};

pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

pub struct ManagedProcess {
    child: Child,
    name: String,
    command: String,
    status: ProcessStatus,
    start_time: Option<DateTime<Utc>>,
    restart_count: u32,
    log_file: PathBuf,
    restart_policy: RestartPolicy,
    max_restarts: u32,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_script(
        &self,
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
    ) -> crate::common::error::Result<()> {
        let log_path = PathBuf::from("logs").join(format!("{}.log", name));
        std::fs::create_dir_all("logs")?;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(stdout) = child.stdout.take() {
            let log_path = log_path.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();

                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 { break; }

                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                    {
                        use std::io::Write;
                        let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
                        writeln!(file, "[{}] {}", timestamp, line.trim()).ok();
                    }

                    line.clear();
                }
            });
        }

        let managed_process = ManagedProcess {
            child,
            name: name.clone(),
            command,
            status: ProcessStatus::Running,
            start_time: Some(Utc::now()),
            restart_count: 0,
            log_file: log_path,
            restart_policy,
            max_restarts,
        };

        let mut processes = self.processes.lock().await;
        processes.insert(name, managed_process);

        Ok(())
    }

    pub async fn stop_script(&self, name: &str) -> crate::common::error::Result<()> {
        let mut processes = self.processes.lock().await;
        if let Some(process) = processes.get_mut(name) {
            process.child.kill().await?;
            process.status = ProcessStatus::Stopped;
            process.start_time = None;
            Ok(())
        } else {
            Err(Error::Process(format!("Script {} not found", name)))
        }
    }

    pub async fn get_status(&self) -> crate::common::error::Result<Vec<ProcessInfo>> {
        let processes = self.processes.lock().await;
        let mut result = Vec::new();

        for (name, process) in processes.iter() {
            let uptime = process.start_time
                .map(|start_time| Utc::now().signed_duration_since(start_time).to_std().unwrap_or_default())
                .unwrap_or_default();

            result.push(ProcessInfo {
                name: name.clone(),
                pid: process.child.id().unwrap_or(0),
                status: process.status.clone(),
                uptime,
                restart_count: process.restart_count,
            });
        }

        Ok(result)
    }

    pub async fn read_logs(&self, name: &str) -> crate::common::error::Result<String> {
        let processes = self.processes.lock().await;
        if let Some(process) = processes.get(name) {
            Ok(tokio::fs::read_to_string(&process.log_file).await?)
        } else {
            Err(Error::Process(format!("Script {} not found", name)))
        }
    }

    pub async fn stop_all(&self) -> crate::common::error::Result<()> {
        let names: Vec<String> = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect()
        };

        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    pub async fn read_all_logs(&self) -> crate::common::error::Result<String> {
        let processes = self.processes.lock().await;
        let mut all_logs = String::new();

        for (name, process) in processes.iter() {
            match tokio::fs::read_to_string(&process.log_file).await {
                Ok(logs) => {
                    all_logs.push_str(&format!("\n=== Logs for {} ===\n", name));
                    all_logs.push_str(&logs);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    all_logs.push_str(&format!("\n=== No logs for {} ===\n", name));
                }
                Err(e) => {
                    all_logs.push_str(&format!("\n=== Error reading logs for {}: {} ===\n", name, e));
                }
            }
        }

        Ok(all_logs)
    }

    pub async fn check_and_restart_if_needed(&self, name: &str) {
        let should_restart = {
            let mut processes = self.processes.lock().await;
            if let Some(process) = processes.get_mut(name) {
                if let Ok(Some(_)) = process.child.try_wait() {
                    match process.restart_policy {
                        RestartPolicy::Always => process.restart_count < process.max_restarts,
                        RestartPolicy::Never => false,
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_restart {
            let process_info = {
                let mut processes = self.processes.lock().await;
                if let Some(process) = processes.get_mut(name) {
                    process.restart_count += 1;
                    (
                        process.command.clone(),
                        process.restart_policy.clone(),
                        process.max_restarts
                    )
                } else {
                    return;
                }
            };

            if let Err(e) = self.start_script(
                name.to_string(),
                process_info.0,
                process_info.1,
                process_info.2
            ).await {
                eprintln!("Failed to restart {}: {}", name, e);
            }
        }
    }

    pub async fn monitor_and_restart(&self) {
        loop {
            let names: Vec<String> = {
                let processes = self.processes.lock().await;
                processes.keys().cloned().collect()
            };

            for name in names {
                self.check_and_restart_if_needed(&name).await;
            }

            sleep(Duration::from_secs(1)).await;
        }
    }
}
