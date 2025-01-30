use turtle_harbor::daemon::server::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = Server::new();
    server.run().await?;
    Ok(())
}
