use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::common::config::{Config, RestartPolicy, Script};
use crate::common::error::Error;
use crate::common::ipc::ProcessStatus;

pub struct ProcessManager {
    config: Config,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}

pub struct ManagedProcess {
    child: Child,
    name: String,
    status: ProcessStatus,
    start_time: Option<DateTime<Utc>>,
    restart_count: u32,
    log_file: PathBuf,
}

impl ProcessManager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_script(&self, name: &str) -> crate::common::error::Result<()> {
        let script = self
            .config
            .scripts
            .get(name)
            .ok_or_else(|| Error::Process(format!("Script {} not found", name)))?;

        std::fs::create_dir_all(&self.config.settings.log_dir)?;
        let log_path = self.config.settings.log_dir.join(format!("{}.log", name));

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&script.command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(stdout) = child.stdout.take() {
            let log_path = log_path.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();

                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }

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
            name: name.to_string(),
            status: ProcessStatus::Running,
            start_time: Some(Utc::now()),
            restart_count: 0,
            log_file: log_path,
        };

        let mut processes = self.processes.lock().await;
        processes.insert(name.to_string(), managed_process);

        println!("Started script: {}", name);
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

    pub async fn get_status(
        &self,
    ) -> crate::common::error::Result<Vec<crate::common::ipc::ProcessInfo>> {
        let processes = self.processes.lock().await;
        let mut result = Vec::new();

        for (name, process) in processes.iter() {
            let uptime = process
                .start_time
                .map(|start_time| {
                    Utc::now()
                        .signed_duration_since(start_time)
                        .to_std()
                        .unwrap_or_default()
                })
                .unwrap_or_default();

            result.push(crate::common::ipc::ProcessInfo {
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
            Err(crate::common::error::Error::Process(format!(
                "Script {} not found",
                name
            )))
        }
    }

    pub async fn start_all(&self) -> crate::common::error::Result<()> {
        let mut processes = self.processes.lock().await;
        for script_name in self.config.scripts.keys() {
            if processes.contains_key(script_name) {
                println!("Script {} is already running", script_name);
                continue;
            }

            drop(processes);
            self.start_script(script_name).await?;
            processes = self.processes.lock().await;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> crate::common::error::Result<()> {
        let mut processes = self.processes.lock().await;
        for script_name in processes.keys().cloned().collect::<Vec<_>>() {
            drop(processes);
            self.stop_script(&script_name).await?;
            processes = self.processes.lock().await;
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
                    all_logs.push_str(&format!(
                        "\n=== Error reading logs for {}: {} ===\n",
                        name, e
                    ));
                }
            }
        }

        Ok(all_logs)
    }
}
