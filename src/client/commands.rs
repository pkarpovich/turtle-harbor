use crate::common::ipc::{self, Command, Response};
use tokio::net::UnixStream;

pub async fn send_command(command: Command) -> crate::common::error::Result<Response> {
    let mut stream = UnixStream::connect(ipc::SOCKET_PATH).await?;

    ipc::send_command(&mut stream, &command).await?;
    let response = ipc::receive_response(&mut stream).await?;

    Ok(response)
}
