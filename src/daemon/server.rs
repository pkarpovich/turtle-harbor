use std::path::Path;
use std::sync::Arc;
use tokio::net::UnixListener;

use crate::common::config::Config;
use crate::common::ipc::{self, Command, Response};
use crate::daemon::process::ProcessManager;

pub struct Server {
    socket_path: String,
    process_manager: Arc<ProcessManager>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self {
            socket_path: ipc::SOCKET_PATH.to_string(),
            process_manager: Arc::new(ProcessManager::new(config)),
        }
    }

    pub async fn run(&self) -> crate::common::error::Result<()> {
        if Path::new(&self.socket_path).exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        println!("Server listening on {}", self.socket_path);

        while let Ok((mut stream, _)) = listener.accept().await {
            let process_manager = Arc::clone(&self.process_manager);

            tokio::spawn(async move {
                while let Ok(command) = ipc::receive_command(&mut stream).await {
                    println!("Received command: {:?}", command);

                    let response = match command {
                        Command::Up { script_name } => match script_name {
                            Some(name) => match process_manager.start_script(&name).await {
                                Ok(_) => Response::Success,
                                Err(e) => Response::Error(e.to_string()),
                            },
                            None => match process_manager.start_all().await {
                                Ok(_) => Response::Success,
                                Err(e) => Response::Error(e.to_string()),
                            },
                        },
                        Command::Down { script_name } => match script_name {
                            Some(name) => match process_manager.stop_script(&name).await {
                                Ok(_) => Response::Success,
                                Err(e) => Response::Error(e.to_string()),
                            },
                            None => match process_manager.stop_all().await {
                                Ok(_) => Response::Success,
                                Err(e) => Response::Error(e.to_string()),
                            },
                        },
                        Command::Ps => match process_manager.get_status().await {
                            Ok(status) => Response::ProcessList(status),
                            Err(e) => Response::Error(e.to_string()),
                        },
                        Command::Logs { script_name } => match script_name {
                            Some(name) => match process_manager.read_logs(&name).await {
                                Ok(logs) => Response::Logs(logs),
                                Err(e) => Response::Error(e.to_string()),
                            },
                            None => match process_manager.read_all_logs().await {
                                Ok(logs) => Response::Logs(logs),
                                Err(e) => Response::Error(e.to_string()),
                            },
                        },
                    };

                    if let Err(e) = ipc::send_response(&mut stream, &response).await {
                        eprintln!("Failed to send response: {}", e);
                        break;
                    }
                }
            });
        }

        Ok(())
    }
}
