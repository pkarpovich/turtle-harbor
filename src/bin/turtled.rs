use std::path::PathBuf;
use turtle_harbor::common::ipc::Profile;
use turtle_harbor::common::logging;
use turtle_harbor::daemon::server::Server;

fn resolve_log_dir() -> PathBuf {
    let brew_var =
        std::env::var("HOMEBREW_VAR").unwrap_or_else(|_| "/opt/homebrew/var".to_string());
    match Profile::current() {
        Profile::Development => PathBuf::from("logs"),
        Profile::Production => PathBuf::from(format!("{}/log/turtle-harbor", brew_var)),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log_dir = resolve_log_dir();
    if !log_dir.exists() {
        std::fs::create_dir_all(&log_dir)?;
    }
    let _log_guard = logging::init_logging(&log_dir);

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

    let mut server = Server::new()?;
    if let Err(e) = server.run().await {
        tracing::error!("Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}
