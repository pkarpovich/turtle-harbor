use std::io::IsTerminal;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init_logging(log_dir: &Path) -> WorkerGuard {
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=info"));

    let file_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=debug"));

    let console_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .with_level(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .compact()
        .with_filter(console_filter);

    let file_appender = rolling::daily(log_dir, "turtle_harbor.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_target(true)
        .with_ansi(false)
        .with_level(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .json()
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}
