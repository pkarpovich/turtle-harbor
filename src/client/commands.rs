use crate::common::error::{Error, Result};
use crate::common::ipc::{self, Command, Response};
use crate::common::paths;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;

pub async fn send_command(command: Command) -> Result<Response> {
    let socket_path = paths::socket_path();
    let mut stream = UnixStream::connect(&socket_path).await?;
    ipc::send_command(&mut stream, &command).await?;

    match timeout(Duration::from_secs(30), ipc::receive_response(&mut stream)).await {
        Ok(response) => response,
        Err(_) => Err(Error::CommandTimeout),
    }
}

pub async fn send_command_follow<F>(command: Command, on_chunk: F) -> Result<()>
where
    F: Fn(&str),
{
    let socket_path = paths::socket_path();
    let mut stream = UnixStream::connect(&socket_path).await?;
    ipc::send_command(&mut stream, &command).await?;

    let initial: Response =
        match timeout(Duration::from_secs(30), ipc::receive_response(&mut stream)).await {
            Ok(response) => response?,
            Err(_) => return Err(Error::CommandTimeout),
        };

    match &initial {
        Response::Logs(content) => on_chunk(content),
        Response::Error(e) => return Err(Error::Io(std::io::Error::other(e.clone()))),
        _ => {}
    }

    loop {
        tokio::select! {
            result = ipc::receive_message::<Response>(&mut stream) => {
                match result {
                    Ok(Response::Logs(chunk)) => on_chunk(&chunk),
                    Ok(Response::Error(e)) => {
                        tracing::error!("Follow error: {}", e);
                        break;
                    }
                    Err(_) => break,
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    Ok(())
}
