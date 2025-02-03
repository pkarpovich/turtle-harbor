use crate::common::error::{Error, Result};
use crate::common::ipc::{self, Command, Response};
use tokio::net::UnixStream;

pub async fn send_command(command: Command) -> Result<Response> {
    let socket_path = ipc::get_socket_path();
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| Error::Io(e))?;

    let mut stream = stream;
    ipc::send_command(&mut stream, &command).await?;
    let response = ipc::receive_response(&mut stream).await?;
    Ok(response)
}
