use turtle_harbor::common::config::Config;
use turtle_harbor::daemon::server::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load("scripts.yml")?;

    let server = Server::new(config);
    server.run().await?;
    Ok(())
}
