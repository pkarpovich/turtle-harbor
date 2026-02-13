use clap::Parser;
use turtle_harbor::common::logging;
use turtle_harbor::common::paths;
use turtle_harbor::daemon::server::Server;

#[derive(Parser)]
#[command(name = "turtled", about = "Turtle Harbor daemon")]
struct Args {
    #[arg(long)]
    http_port: Option<u16>,

    #[arg(long, default_value = "0.0.0.0")]
    http_bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let log_dir = paths::log_dir();
    paths::ensure_dir(&log_dir)?;
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

    let mut server = Server::new(args.http_port, args.http_bind)?;
    if let Err(e) = server.run().await {
        tracing::error!("Server error: {}", e);
        return Err(e.into());
    }

    Ok(())
}
