use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};

use crate::common::ipc::{self, Command, Response};
use crate::common::error::Result;
use crate::daemon::process::ProcessManager;

pub struct Server {
    socket_path: String,
    process_manager: Arc<ProcessManager>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            socket_path: ipc::get_socket_path().to_string(),
            process_manager: Arc::new(ProcessManager::new()),
        }
    }

    async fn handle_command(process_manager: &ProcessManager, command: Command) -> Response {
        match command {
            Command::Up { name, command, restart_policy, max_restarts } => {
                match process_manager.start_script(name, command, restart_policy, max_restarts).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            },
            Command::Down { name } => {
                match process_manager.stop_script(&name).await {
                    Ok(_) => Response::Success,
                    Err(e) => Response::Error(e.to_string()),
                }
            },
            Command::Ps => {
                match process_manager.get_status().await {
                    Ok(status) => Response::ProcessList(status),
                    Err(e) => Response::Error(e.to_string()),
                }
            },
            Command::Logs { name } => {
                match process_manager.read_logs(&name).await {
                    Ok(logs) => Response::Logs(logs),
                    Err(e) => Response::Error(e.to_string()),
                }
            }
        }
    }

    async fn handle_client(process_manager: Arc<ProcessManager>, mut stream: UnixStream) {
        while let Ok(command) = ipc::receive_command(&mut stream).await {
            println!("Received command: {:?}", command);

            let response = Self::handle_command(&process_manager, command).await;
            if let Err(e) = ipc::send_response(&mut stream, &response).await {
                eprintln!("Failed to send response: {}", e);
                break;
            }
        }
    }

    async fn setup_listener(&self) -> Result<UnixListener> {
        if Path::new(&self.socket_path).exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        println!("Server listening on {}", self.socket_path);
        Ok(listener)
    }

    async fn setup_signal_handlers() -> Result<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)> {
        Ok((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ))
    }

    async fn start_process_monitor(&self) {
        let process_manager_clone = Arc::clone(&self.process_manager);
        tokio::spawn(async move {
            process_manager_clone.monitor_and_restart().await;
        });
    }

    pub async fn run(&self) -> Result<()> {
        let listener = self.setup_listener().await?;
        let (mut sigterm, mut sigint) = Self::setup_signal_handlers().await?;

        self.start_process_monitor().await;

        let socket_path = self.socket_path.clone();
        let process_manager = Arc::clone(&self.process_manager);

        tokio::select! {
            _ = async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let process_manager = Arc::clone(&process_manager);
                    tokio::spawn(async move {
                        Self::handle_client(process_manager, stream).await;
                    });
                }
            } => {}

            _ = sigterm.recv() => {
                println!("Received SIGTERM, shutting down...");
            }
            _ = sigint.recv() => {
                println!("Received SIGINT, shutting down...");
            }
        }

        self.shutdown(&socket_path).await
    }

    async fn shutdown(&self, socket_path: &str) -> Result<()> {
        self.process_manager.stop_all().await?;
        if Path::new(socket_path).exists() {
            std::fs::remove_file(socket_path)?;
        }
        println!("Shutdown complete");
        Ok(())
    }
}