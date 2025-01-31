use crate::common::error::Result;
use crate::common::ipc::{self, Command, Response};
use tokio::net::UnixStream;

pub async fn send_command(command: Command) -> Result<Response> {
    let socket_path = ipc::get_socket_path();
    let mut stream = UnixStream::connect(socket_path).await?;
    ipc::send_command(&mut stream, &command).await?;
    let response = ipc::receive_response(&mut stream).await?;
    Ok(response)
}
