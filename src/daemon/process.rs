use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::config::RestartPolicy;
use crate::common::error::{Error, Result};
use crate::common::ipc::{ProcessInfo, ProcessStatus};

struct ProcessConfig {
    name: String,
    command: String,
    restart_policy: RestartPolicy,
    max_restarts: u32,
}

struct ProcessOutput {
    child: Child,
}

pub struct ManagedProcess {
    child: Child,
    command: String,
    status: ProcessStatus,
    start_time: Option<DateTime<Utc>>,
    restart_count: u32,
    log_file: PathBuf,
    restart_policy: RestartPolicy,
    max_restarts: u32,
}

pub struct ProcessManager {
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn spawn_log_reader<R>(mut reader: BufReader<R>, log_path: PathBuf)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                let bytes = reader.read_line(&mut line).await.unwrap_or(0);
                if bytes == 0 {
                    break;
                }
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
                    use std::io::Write;
                    writeln!(file, "[{}] {}", timestamp, line.trim()).ok();
                }
                line.clear();
            }
        });
    }

    async fn setup_process_output(command: &str, log_path: &PathBuf) -> Result<ProcessOutput> {
        let log_dir = log_path.parent().unwrap_or_else(|| Path::new("logs"));
        std::fs::create_dir_all(log_dir)?;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            Self::spawn_log_reader(reader, log_path.clone()).await;
        }
        if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            Self::spawn_log_reader(reader, log_path.clone()).await;
        }
        Ok(ProcessOutput { child })
    }

    pub async fn start_script(
        &self,
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
    ) -> Result<()> {
        let config = ProcessConfig {
            name: name.clone(),
            command: command.clone(),
            restart_policy,
            max_restarts,
        };
        let log_path = PathBuf::from("logs").join(format!("{}.log", name));
        let output = Self::setup_process_output(&command, &log_path).await?;
        let managed_process = ManagedProcess {
            child: output.child,
            command: config.command,
            status: ProcessStatus::Running,
            start_time: Some(Utc::now()),
            restart_count: 0,
            log_file: log_path,
            restart_policy: config.restart_policy,
            max_restarts: config.max_restarts,
        };
        let mut processes = self.processes.lock().await;
        processes.insert(config.name, managed_process);
        Ok(())
    }

    pub async fn stop_script(&self, name: &str) -> Result<()> {
        let mut processes = self.processes.lock().await;
        match processes.get_mut(name) {
            Some(process) => {
                process.child.kill().await?;
                process.status = ProcessStatus::Stopped;
                process.start_time = None;
                Ok(())
            }
            None => Err(Error::Process(format!("Script {} not found", name))),
        }
    }

    pub async fn get_status(&self) -> Result<Vec<ProcessInfo>> {
        let processes = self.processes.lock().await;
        Ok(processes
            .iter()
            .map(|(name, process)| {
                let uptime = process
                    .start_time
                    .map(|start_time| {
                        Utc::now()
                            .signed_duration_since(start_time)
                            .to_std()
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                ProcessInfo {
                    name: name.clone(),
                    pid: process.child.id().unwrap_or(0),
                    status: process.status.clone(),
                    uptime,
                    restart_count: process.restart_count,
                }
            })
            .collect())
    }

    pub async fn read_logs(&self, name: &str) -> Result<String> {
        let processes = self.processes.lock().await;
        match processes.get(name) {
            Some(process) => Ok(tokio::fs::read_to_string(&process.log_file).await?),
            None => Err(Error::Process(format!("Script {} not found", name))),
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let names: Vec<String> = {
            let processes = self.processes.lock().await;
            processes.keys().cloned().collect()
        };
        for name in names {
            self.stop_script(&name).await?;
        }
        Ok(())
    }

    pub async fn read_all_logs(&self) -> Result<String> {
        let processes = self.processes.lock().await;
        let mut all_logs = String::new();
        for (name, process) in processes.iter() {
            let log_content = match tokio::fs::read_to_string(&process.log_file).await {
                Ok(logs) => format!("\n=== Logs for {} ===\n{}", name, logs),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    format!("\n=== No logs for {} ===\n", name)
                }
                Err(e) => format!("\n=== Error reading logs for {}: {} ===\n", name, e),
            };
            all_logs.push_str(&log_content);
        }
        Ok(all_logs)
    }

    async fn should_restart(&self, name: &str) -> Option<(String, RestartPolicy, u32)> {
        let mut processes = self.processes.lock().await;
        processes.get_mut(name).and_then(|process| {
            if let Ok(Some(_)) = process.child.try_wait() {
                match process.restart_policy {
                    RestartPolicy::Always if process.restart_count < process.max_restarts => {
                        process.restart_count += 1;
                        Some((
                            process.command.clone(),
                            process.restart_policy.clone(),
                            process.max_restarts,
                        ))
                    }
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    pub async fn check_and_restart_if_needed(&self, name: &str) {
        if let Some((command, policy, max_restarts)) = self.should_restart(name).await {
            if let Err(e) = self
                .start_script(name.to_string(), command, policy, max_restarts)
                .await
            {
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
