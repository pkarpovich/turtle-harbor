use crate::common::error::Result;
use crate::common::ipc::{self, Command, Profile, Response};
use crate::daemon::process::ProcessManager;
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
        let state_file = match Profile::current() {
            Profile::Development => PathBuf::from("/tmp/turtle-harbor-state.json"),
            Profile::Production => PathBuf::from("/var/lib/turtle-harbor/state.json"),
        };

        println!("Starting server with state file: {:?}", state_file);
        let process_manager = match ProcessManager::new(state_file) {
            Ok(pm) => {
                println!("Process manager initialized successfully");
                pm
            }
            Err(e) => {
                eprintln!("Failed to initialize process manager: {}", e);
                return Err(e);
            }
        };

        println!("Creating server instance");
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
            println!("Received command: {:?}", command);
            let response = Self::handle_command(&process_manager, command).await;
            if let Err(e) = ipc::send_response(&mut stream, &response).await {
                eprintln!("Failed to send response: {}", e);
                break;
            }
        }
    }

    async fn accept_loop(listener: UnixListener, process_manager: Arc<ProcessManager>) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let pm = Arc::clone(&process_manager);
                    tokio::spawn(async move {
                        Self::handle_client(pm, stream).await;
                    });
                }
                Err(e) => {
                    eprintln!("Accept error: {}", e);
                    break;
                }
            }
        }
    }

    async fn setup_listener(&self) -> Result<UnixListener> {
        if Path::new(&self.socket_path).exists() {
            println!("Removing existing socket file");
            std::fs::remove_file(&self.socket_path)?;
        }
        println!("Creating new socket listener at {}", self.socket_path);
        let listener = UnixListener::bind(&self.socket_path)?;
        println!("Server listening on {}", self.socket_path);
        Ok(listener)
    }

    async fn setup_signal_handlers(
    ) -> Result<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)> {
        Ok((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ))
    }

    async fn start_process_monitor(&self) {
        let pm = Arc::clone(&self.process_manager);
        tokio::spawn(async move {
            pm.monitor_and_restart().await;
        });
    }

    pub async fn run(&self) -> Result<()> {
        println!("Starting server initialization");

        match self.process_manager.restore_state().await {
            Ok(_) => println!("State restored successfully"),
            Err(e) => {
                eprintln!("Failed to restore state: {}", e);
                return Err(e);
            }
        }

        println!("Setting up socket listener");
        let listener = match self.setup_listener().await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Failed to setup listener: {}", e);
                return Err(e);
            }
        };

        println!("Setting up signal handlers");
        let (mut sigterm, mut sigint) = match Self::setup_signal_handlers().await {
            Ok(handlers) => handlers,
            Err(e) => {
                eprintln!("Failed to setup signal handlers: {}", e);
                return Err(e);
            }
        };

        println!("Starting process monitor");
        self.start_process_monitor().await;

        let process_manager = Arc::clone(&self.process_manager);
        println!("Starting main server loop");
        tokio::select! {
            result = Self::accept_loop(listener, process_manager) => {
                println!("Accept loop terminated: {:?}", result);
            },
            _ = sigterm.recv() => {
                println!("Received SIGTERM, shutting down...");
            }
            _ = sigint.recv() => {
                println!("Received SIGINT, shutting down...");
            }
        }

        println!("Starting shutdown sequence");
        self.shutdown().await
    }

    async fn shutdown(&self) -> Result<()> {
        self.process_manager.shutdown().await?;

        if Path::new(&self.socket_path).exists() {
            std::fs::remove_file(&self.socket_path)?;
        }
        println!("Shutdown complete");
        Ok(())
    }
}
