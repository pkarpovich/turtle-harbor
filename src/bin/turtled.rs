use tracing_subscriber;
use turtle_harbor::daemon::server::Server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("Daemon panicked: {}", panic_info);
        if let Some(location) = panic_info.location() {
            eprintln!(
                "Panic occurred in file '{}' at line {}",
                location.file(),
                location.line()
            );
        }
    }));

    tracing_subscriber::fmt::init();

    let server = Server::new()?;
    if let Err(e) = server.run().await {
        eprintln!("Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}
