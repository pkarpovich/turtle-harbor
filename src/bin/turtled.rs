use turtle_harbor::daemon::server::Server;
use turtle_harbor::common::logging;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _log_guard = logging::init_logging();

    tracing::info!("Starting turtle-harbor daemon");
    std::panic::set_hook(Box::new(|panic_info| {
        tracing::error!("Daemon panicked: {}", panic_info);
        if let Some(location) = panic_info.location() {
            tracing::error!(
                "Panic occurred in file '{}' at line {}",
                location.file(),
                location.line()
            );
        }
    }));

    let server = Server::new()?;
    if let Err(e) = server.run().await {
        tracing::error!("Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}
