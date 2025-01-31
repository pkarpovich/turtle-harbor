use tracing_subscriber;
use turtle_harbor::daemon::server::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let server = Server::new();
    server.run().await?;

    Ok(())
}
