use crate::common::error::{Error, Result};
use crate::common::ipc::{self, Command, Response};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;

pub async fn send_command(command: Command) -> Result<Response> {
    let socket_path = ipc::get_socket_path();
    let mut stream = UnixStream::connect(socket_path).await?;
    ipc::send_command(&mut stream, &command).await?;

    match timeout(Duration::from_secs(30), ipc::receive_response(&mut stream)).await {
        Ok(response) => response,
        Err(_) => Err(Error::CommandTimeout),
    }
}
