use crate::common::error::{Error, Result};
use crate::common::config::RestartPolicy;
use serde::{Deserialize, Serialize};
use serde_json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Command {
    Up {
        name: String,
        command: String,
        restart_policy: RestartPolicy,
        max_restarts: u32,
    },
    Down { name: String },
    Ps,
    Logs { name: String },
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

pub const SOCKET_PATH: &str = "/tmp/turtle-harbor.sock";

pub async fn send_command(
    stream: &mut UnixStream,
    command: &Command,
) -> Result<()> {
    let data = serde_json::to_vec(command)?;
    let len = data.len() as u32;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

pub async fn receive_command(stream: &mut UnixStream) -> crate::common::error::Result<Command> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);

    let mut buffer = vec![0u8; len as usize];
    stream.read_exact(&mut buffer).await?;

    let command = serde_json::from_slice(&buffer)?;
    Ok(command)
}

pub async fn send_response(
    stream: &mut UnixStream,
    response: &Response,
) -> Result<()> {
    let data = serde_json::to_vec(response)?;
    let len = data.len() as u32;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&data).await?;
    Ok(())
}

pub async fn receive_response(stream: &mut UnixStream) -> crate::common::error::Result<Response> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);

    let mut buffer = vec![0u8; len as usize];
    stream.read_exact(&mut buffer).await?;

    let response = serde_json::from_slice(&buffer)?;
    Ok(response)
}
