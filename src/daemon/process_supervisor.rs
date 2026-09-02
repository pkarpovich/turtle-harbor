use crate::common::config::Script;
use crate::common::error::Result;
use crate::common::ipc::{ProcessInfo, ProcessStatus};
use crate::daemon::daemon_core::DaemonEvent;
use crate::daemon::log_monitor::{self, ScriptLogger};
use crate::daemon::loki_shipper::LokiLogEntry;
use crate::daemon::process::{is_process_alive, ManagedProcess, ScriptStartResult};
use chrono::Local;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::Command as TokioCommand;
use tokio::sync::{broadcast, mpsc};

pub struct StartScript<'a> {
    pub name: &'a str,
    pub script: &'a Script,
    pub broadcast_tx: broadcast::Sender<String>,
    pub config_dir: &'a Path,
    pub extra_env: HashMap<OsString, OsString>,
}

pub struct ProcessSupervisor {
    processes: HashMap<String, ManagedProcess>,
    event_tx: mpsc::Sender<DaemonEvent>,
    log_dir: PathBuf,
    loki_tx: Option<mpsc::Sender<LokiLogEntry>>,
    next_instance_id: u64,
}

impl ProcessSupervisor {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>, log_dir: PathBuf) -> Self {
        Self {
            processes: HashMap::new(),
            event_tx,
            log_dir,
            loki_tx: None,
            next_instance_id: 1,
        }
    }

    pub fn set_log_dir(&mut self, log_dir: PathBuf) {
        self.log_dir = log_dir;
    }

    pub fn set_loki_tx(&mut self, tx: mpsc::Sender<LokiLogEntry>) {
        self.loki_tx = Some(tx);
    }

    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub fn start_script(&mut self, options: StartScript<'_>) -> Result<ScriptStartResult> {
        let StartScript {
            name,
            script: script_def,
            broadcast_tx,
            config_dir,
            extra_env,
        } = options;

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
        let logger = ScriptLogger::new(
            log_path,
            broadcast_tx,
            name.to_string(),
            self.loki_tx.clone(),
        )?;

        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-c")
            .arg(&canonical_command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let working_dir = script_def
            .resolved_context(config_dir)
            .unwrap_or_else(|| config_dir.to_path_buf());
        tracing::debug!(script = %name, working_dir = %working_dir.display(), "Setting working directory");
        cmd.current_dir(&working_dir);

        let mut env = script_def.resolved_env(config_dir);
        for (key, value) in extra_env {
            env.insert(key, value);
        }
        if !env.is_empty() {
            tracing::debug!(script = %name, env_keys = ?env.keys().collect::<Vec<_>>(), "Injecting environment");
            cmd.envs(&env);
        }

        // SAFETY: pre_exec runs setpgid(0,0) in the forked child before exec — this is
        // async-signal-safe per POSIX and only affects the child process
        let mut child = unsafe {
            cmd.pre_exec(|| {
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

        let instance_id = self.next_instance_id;
        self.next_instance_id = self.next_instance_id.wrapping_add(1).max(1);

        let event_tx = self.event_tx.clone();
        let watcher_name = name.to_string();
        let watcher = tokio::spawn(async move {
            let status = child.wait().await;
            let _ = event_tx
                .send(DaemonEvent::ProcessExited {
                    name: watcher_name,
                    instance_id,
                    status: status.ok(),
                })
                .await;
        });

        self.processes.insert(
            name.to_string(),
            ManagedProcess {
                pid,
                instance_id,
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
                // SAFETY: pgid is a valid process group ID set via setpgid in pre_exec
                unsafe { libc::killpg(pgid, libc::SIGTERM) };

                if let Some(mut watcher) = process.watcher.take() {
                    match tokio::time::timeout(Duration::from_secs(5), &mut watcher).await {
                        Ok(_) => {
                            tracing::debug!(script = %name, "Process group exited after SIGTERM")
                        }
                        Err(_) => {
                            tracing::warn!(script = %name, "SIGTERM timeout, sending SIGKILL to process group");
                            // SAFETY: same pgid, escalating to SIGKILL after SIGTERM timeout
                            unsafe { libc::killpg(pgid, libc::SIGKILL) };
                            if tokio::time::timeout(Duration::from_secs(2), &mut watcher)
                                .await
                                .is_err()
                            {
                                tracing::warn!(
                                    script = %name,
                                    "Process did not exit after SIGKILL, aborting watcher"
                                );
                                watcher.abort();
                            } else {
                                tracing::debug!(script = %name, "Process group exited after SIGKILL");
                            }
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
                    // SAFETY: pgid is a valid process group ID set via setpgid in pre_exec
                    unsafe { libc::killpg(pgid, libc::SIGTERM) };
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    if is_process_alive(process.pid) {
                        // SAFETY: same pgid, escalating to SIGKILL after SIGTERM timeout
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
                    status: process.status,
                    uptime,
                    restart_count: process.restart_count,
                    exit_code: None,
                    config_path: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::RestartPolicy;
    use tempfile::TempDir;

    fn make_script(command: String, env: Option<HashMap<String, String>>) -> Script {
        Script {
            command,
            restart_policy: RestartPolicy::Never,
            max_restarts: None,
            cron: None,
            context: None,
            venv: None,
            env,
            env_file: None,
            nats: None,
        }
    }

    async fn run_probe(script: Script, extra_env: HashMap<OsString, OsString>) -> String {
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("probe.txt");
        let mut extra_env = extra_env;
        extra_env.insert(OsString::from("OUT"), OsString::from(&out));

        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
        let mut supervisor = ProcessSupervisor::new(event_tx, tmp.path().join("logs"));

        let result = supervisor
            .start_script(StartScript {
                name: "probe",
                script: &script,
                broadcast_tx,
                config_dir: tmp.path(),
                extra_env,
            })
            .unwrap();
        match result {
            ScriptStartResult::Started => {}
            ScriptStartResult::AlreadyRunning => panic!("probe script was already running"),
        }

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let DaemonEvent::ProcessExited {
            name,
            instance_id: _,
            status: _,
        } = event
        else {
            panic!("expected a ProcessExited event");
        };
        assert_eq!(name, "probe");

        std::fs::read_to_string(&out).unwrap()
    }

    #[tokio::test]
    async fn extra_env_reaches_the_process() {
        let script = make_script(r#"printf "%s" "$TH_PROBE" > "$OUT""#.to_string(), None);
        let mut extra_env = HashMap::new();
        extra_env.insert(OsString::from("TH_PROBE"), OsString::from("from_job"));

        assert_eq!(run_probe(script, extra_env).await, "from_job");
    }

    #[tokio::test]
    async fn extra_env_overrides_script_env() {
        let mut env = HashMap::new();
        env.insert("TH_PROBE".to_string(), "from_script_env".to_string());
        let script = make_script(r#"printf "%s" "$TH_PROBE" > "$OUT""#.to_string(), Some(env));
        let mut extra_env = HashMap::new();
        extra_env.insert(OsString::from("TH_PROBE"), OsString::from("from_job"));

        assert_eq!(run_probe(script, extra_env).await, "from_job");
    }
}
