use crate::common::error::Result;
use crate::common::ipc::{self, Profile};
use crate::daemon::daemon_core::{DaemonCore, DaemonEvent};
use std::path::PathBuf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

pub struct Server {
    socket_path: PathBuf,
    daemon_core: DaemonCore,
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

        let log_dir = match Profile::current() {
            Profile::Development => PathBuf::from("logs"),
            Profile::Production => PathBuf::from(format!("{}/log/turtle-harbor", brew_var)),
        };

        let daemon_core = DaemonCore::new(state_file, log_dir)?;
        tracing::info!("DaemonCore initialized successfully");

        Ok(Self {
            socket_path: PathBuf::from(ipc::get_socket_path()),
            daemon_core,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let listener = self.setup_listener()?;
        let event_tx = self.daemon_core.event_tx();

        tokio::spawn(accept_loop(listener, event_tx.clone()));
        tokio::spawn(signal_handler(event_tx));

        self.daemon_core.run().await?;

        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }
        tracing::info!("Shutdown complete");
        Ok(())
    }

    fn setup_listener(&self) -> Result<UnixListener> {
        if self.socket_path.exists() {
            tracing::info!(path = ?self.socket_path, "Removing existing socket file");
            std::fs::remove_file(&self.socket_path)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        tracing::info!(path = ?self.socket_path, "Server listening");
        Ok(listener)
    }
}

async fn accept_loop(listener: UnixListener, event_tx: mpsc::Sender<DaemonEvent>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(addr = ?addr, "Accepted new connection");
                let tx = event_tx.clone();
                tokio::spawn(handle_client(stream, tx));
            }
            Err(e) => {
                tracing::error!(error = ?e, "Accept error");
                break;
            }
        }
    }
}

async fn handle_client(mut stream: UnixStream, event_tx: mpsc::Sender<DaemonEvent>) {
    while let Ok(command) = ipc::receive_command(&mut stream).await {
        tracing::info!(command = ?command, "Received command");
        let (reply_tx, reply_rx) = oneshot::channel();

        if event_tx
            .send(DaemonEvent::ClientCommand { command, reply_tx })
            .await
            .is_err()
        {
            tracing::error!("Failed to send command to daemon core");
            break;
        }

        match reply_rx.await {
            Ok(response) => {
                if let Err(e) = ipc::send_response(&mut stream, &response).await {
                    tracing::error!(error = ?e, "Failed to send response");
                    break;
                }
            }
            Err(_) => {
                tracing::error!("Daemon core dropped reply channel");
                break;
            }
        }
    }
}

async fn signal_handler(event_tx: mpsc::Sender<DaemonEvent>) {
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down..."),
        _ = sigint.recv() => tracing::info!("Received SIGINT, shutting down..."),
    }

    let _ = event_tx.send(DaemonEvent::Shutdown).await;
}
