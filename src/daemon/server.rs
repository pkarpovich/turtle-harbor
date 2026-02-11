use crate::common::error::Result;
use crate::common::ipc::{self, Command, Profile, Response};
use crate::daemon::daemon_core::{DaemonCore, DaemonEvent, LogChannels};
use crate::daemon::health::HealthSnapshot;
use crate::daemon::http_server;
use std::path::PathBuf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot, watch};

pub struct Server {
    socket_path: PathBuf,
    daemon_core: DaemonCore,
    log_channels: LogChannels,
    health: HealthSnapshot,
    http_port: Option<u16>,
    http_bind: String,
    shutdown_tx: watch::Sender<bool>,
}

impl Server {
    pub fn new(http_port: Option<u16>, http_bind: String) -> Result<Self> {
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
        let log_channels = daemon_core.log_channels();
        let health = daemon_core.health_snapshot();
        let (shutdown_tx, _) = watch::channel(false);
        tracing::info!("DaemonCore initialized successfully");

        Ok(Self {
            socket_path: PathBuf::from(ipc::get_socket_path()),
            daemon_core,
            log_channels,
            health,
            http_port,
            http_bind,
            shutdown_tx,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let listener = self.setup_listener()?;
        let event_tx = self.daemon_core.event_tx();
        let log_channels = self.log_channels.clone();

        tokio::spawn(accept_loop(listener, event_tx.clone(), log_channels));
        tokio::spawn(signal_handler(event_tx));

        if let Some(port) = self.http_port {
            let health = self.health.clone();
            let bind = self.http_bind.clone();
            let shutdown_rx = self.shutdown_tx.subscribe();
            tokio::spawn(http_server::run_http_server(
                bind,
                port,
                health,
                shutdown_rx,
            ));
        }

        self.daemon_core.run().await?;

        let _ = self.shutdown_tx.send(true);

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

async fn accept_loop(
    listener: UnixListener,
    event_tx: mpsc::Sender<DaemonEvent>,
    log_channels: LogChannels,
) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tracing::debug!(addr = ?addr, "Accepted new connection");
                let tx = event_tx.clone();
                let lc = log_channels.clone();
                tokio::spawn(handle_client(stream, tx, lc));
            }
            Err(e) => {
                tracing::error!(error = ?e, "Accept error");
                break;
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream,
    event_tx: mpsc::Sender<DaemonEvent>,
    log_channels: LogChannels,
) {
    while let Ok(command) = ipc::receive_command(&mut stream).await {
        tracing::info!(command = ?command, "Received command");

        let follow_name = match &command {
            Command::Logs { name, follow: true, .. } => name.clone(),
            _ => None,
        };

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

        if let Some(name) = follow_name {
            follow_via_broadcast(&mut stream, &log_channels, &name).await;
            break;
        }
    }
}

async fn follow_via_broadcast(stream: &mut UnixStream, log_channels: &LogChannels, name: &str) {
    let rx = log_channels
        .lock()
        .ok()
        .and_then(|channels| channels.get(name).map(|tx| tx.subscribe()));
    let mut rx = match rx {
        Some(rx) => rx,
        None => {
            let response = Response::Error(format!("No active log channel for '{}'", name));
            let _ = ipc::send_response(stream, &response).await;
            return;
        }
    };

    loop {
        match rx.recv().await {
            Ok(line) => {
                let response = Response::Logs(line);
                if ipc::send_response(stream, &response).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(script = %name, skipped = n, "Follow client lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
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
