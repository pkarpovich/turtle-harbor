use crate::common::error::Result;
use crate::common::ipc::{self, Command, Profile, Response};
use crate::daemon::process::ProcessManager;
use crate::daemon::process_monitor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};

pub struct Server {
    socket_path: String,
    process_manager: Arc<ProcessManager>,
}

impl Server {
    pub fn new() -> Result<Self> {
        tracing::info!("Starting server initialization");
        let brew_var =
            std::env::var("HOMEBREW_VAR").unwrap_or_else(|_| "/opt/homebrew/var".to_string());
        let state_file = match Profile::current() {
            Profile::Development => PathBuf::from("/tmp/turtle-harbor-state.json"),
            Profile::Production => {
                PathBuf::from(format!("{}/lib/turtle-harbor/state.json", brew_var))
            }
        };

        tracing::info!(state_file = ?state_file, "Using state file");
        let process_manager = match ProcessManager::new(state_file) {
            Ok(pm) => {
                tracing::info!("Process manager initialized successfully");
                pm
            }
            Err(e) => {
                tracing::error!(error = ?e, "Failed to initialize process manager");
                return Err(e);
            }
        };

        Ok(Self {
            socket_path: ipc::get_socket_path().to_string(),
            process_manager: Arc::new(process_manager),
        })
    }

    async fn handle_command(process_manager: &ProcessManager, command: Command) -> Response {
        match command {
            Command::Up {
                name,
                command,
                restart_policy,
                max_restarts,
            } => match process_manager
                .start_script(name, command, restart_policy, max_restarts)
                .await
            {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Down { name } => match process_manager.stop_script(&name).await {
                Ok(_) => Response::Success,
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Ps => match process_manager.get_status().await {
                Ok(status) => Response::ProcessList(status),
                Err(e) => Response::Error(e.to_string()),
            },
            Command::Logs { name } => match process_manager.read_logs(&name).await {
                Ok(logs) => Response::Logs(logs),
                Err(e) => Response::Error(e.to_string()),
            },
        }
    }

    async fn handle_client(process_manager: Arc<ProcessManager>, mut stream: UnixStream) {
        while let Ok(command) = ipc::receive_command(&mut stream).await {
            tracing::info!(command = ?command, "Received command");
            let response = Self::handle_command(&process_manager, command).await;
            if let Err(e) = ipc::send_response(&mut stream, &response).await {
                tracing::error!(error = ?e, "Failed to send response");
                break;
            }
        }
    }

    async fn accept_loop(listener: UnixListener, process_manager: Arc<ProcessManager>) {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    tracing::debug!(addr = ?addr, "Accepted new connection");
                    let pm = Arc::clone(&process_manager);
                    tokio::spawn(async move {
                        Self::handle_client(pm, stream).await;
                    });
                }
                Err(e) => {
                    tracing::error!(error = ?e, "Accept error");
                    break;
                }
            }
        }
    }

    async fn setup_listener(&self) -> Result<UnixListener> {
        if Path::new(&self.socket_path).exists() {
            tracing::info!(path = ?self.socket_path, "Removing existing socket file");
            std::fs::remove_file(&self.socket_path)?;
        }
        tracing::info!(path = ?self.socket_path, "Creating new socket listener");
        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(path = ?self.socket_path, "Server listening");
        Ok(listener)
    }

    async fn setup_signal_handlers(
    ) -> Result<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)> {
        tracing::debug!("Setting up signal handlers");
        Ok((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ))
    }

    async fn start_process_monitor(&self) {
        let processes = self.process_manager.get_processes();
        let pm = Arc::clone(&self.process_manager);

        let restart_handler = Arc::new(move |name, command, policy, max_restarts| {
            let pm = Arc::clone(&pm);
            async move { pm.start_script(name, command, policy, max_restarts).await }
        });

        tracing::info!("Starting process monitor using process_monitor module");
        tokio::spawn(async move {
            process_monitor::monitor_and_restart(processes, restart_handler).await;
        });
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Starting server initialization");

        match self.process_manager.restore_state().await {
            Ok(_) => tracing::info!("State restored successfully"),
            Err(e) => {
                tracing::error!(error = ?e, "Failed to restore state");
                return Err(e);
            }
        }

        tracing::info!("Setting up socket listener");
        let listener = match self.setup_listener().await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = ?e, "Failed to setup listener");
                return Err(e);
            }
        };

        tracing::info!("Setting up signal handlers");
        let (mut sigterm, mut sigint) = match Self::setup_signal_handlers().await {
            Ok(handlers) => handlers,
            Err(e) => {
                tracing::error!(error = ?e, "Failed to setup signal handlers");
                return Err(e);
            }
        };

        tracing::info!("Starting process monitor");
        self.start_process_monitor().await;

        let process_manager = Arc::clone(&self.process_manager);
        tracing::info!("Starting main server loop");
        tokio::select! {
            result = Self::accept_loop(listener, process_manager) => {
               tracing::info!(result = ?result, "Accept loop terminated");
            },
            _ = sigterm.recv() => {
               tracing::info!("Received SIGTERM, shutting down...");
            }
            _ = sigint.recv() => {
               tracing::info!("Received SIGINT, shutting down...");
            }
        }

        tracing::info!("Starting shutdown sequence");
        self.shutdown().await
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("Shutting down processes");
        self.process_manager.shutdown().await?;

        if Path::new(&self.socket_path).exists() {
            tracing::info!("Removing socket file");
            std::fs::remove_file(&self.socket_path)?;
        }
        tracing::info!("Shutdown complete");
        Ok(())
    }
}
