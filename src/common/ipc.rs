use crate::common::config::RestartPolicy;
use crate::common::error::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Clone, PartialEq)]
pub enum Profile {
    Development,
    Production,
}

impl Profile {
    pub fn current() -> Self {
        if cfg!(debug_assertions) {
            Profile::Development
        } else {
            Profile::Production
        }
    }
}

pub fn get_socket_path() -> &'static str {
    const PROD_SOCKET_PATH: &str = "/opt/homebrew/var/run/turtle-harbor.sock";
    const DEV_SOCKET_PATH: &str = "/tmp/turtle-harbor.sock";

    let current_profile = Profile::current();
    let path = match current_profile {
        Profile::Development => DEV_SOCKET_PATH,
        Profile::Production => PROD_SOCKET_PATH,
    };
    tracing::debug!(socket_path = path, "Using socket path");
    path
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Command {
    Up {
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
    },
    Down {
        name: String,
    },
    Ps,
    Logs {
        name: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Response {
    Success,
    Error(String),
    ProcessList(Vec<ProcessInfo>),
    Logs(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProcessInfo {
    pub name: String,
    pub pid: u32,
    pub status: ProcessStatus,
    pub uptime: Duration,
    pub restart_count: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum ProcessStatus {
    Running,
    Stopped,
    Failed,
}

async fn send_message<T: Serialize>(stream: &mut UnixStream, message: &T) -> Result<()> {
    let data = serde_json::to_vec(message)?;
    let len = data.len() as u32;
    tracing::trace!(message_len = len, "Sending message");
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

async fn receive_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    tracing::trace!(message_len = len, "Receiving message");

    let mut buffer = vec![0u8; len as usize];
    stream.read_exact(&mut buffer).await?;

    let message = serde_json::from_slice(&buffer)?;
    tracing::trace!("Message deserialized successfully");
    Ok(message)
}

pub async fn send_command(stream: &mut UnixStream, command: &Command) -> Result<()> {
    tracing::debug!(command = ?command, "Sending command");
    send_message(stream, command).await
}

pub async fn receive_command(stream: &mut UnixStream) -> Result<Command> {
    let command = receive_message(stream).await?;
    tracing::debug!(command = ?command, "Received command");
    Ok(command)
}

pub async fn send_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    tracing::debug!(response = ?response, "Sending response");
    send_message(stream, response).await
}

pub async fn receive_response(stream: &mut UnixStream) -> Result<Response> {
    let response = receive_message(stream).await?;
    tracing::debug!(response = ?response, "Received response");
    Ok(response)
}
