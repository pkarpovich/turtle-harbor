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
    const PROD_SOCKET_PATH: &str = "/var/run/turtle-harbor.sock";
    const DEV_SOCKET_PATH: &str = "/tmp/turtle-harbor.sock";

    let current_profile = Profile::current();
    eprintln!("Using profile: {:?}", current_profile);
    match current_profile {
        Profile::Development => DEV_SOCKET_PATH,
        Profile::Production => PROD_SOCKET_PATH,
    }
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
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

async fn receive_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);

    let mut buffer = vec![0u8; len as usize];
    stream.read_exact(&mut buffer).await?;

    Ok(serde_json::from_slice(&buffer)?)
}

pub async fn send_command(stream: &mut UnixStream, command: &Command) -> Result<()> {
    send_message(stream, command).await
}

pub async fn receive_command(stream: &mut UnixStream) -> Result<Command> {
    receive_message(stream).await
}

pub async fn send_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    send_message(stream, response).await
}

pub async fn receive_response(stream: &mut UnixStream) -> Result<Response> {
    receive_message(stream).await
}
