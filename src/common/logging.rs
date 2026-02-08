use std::io::IsTerminal;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init_logging() -> WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("turtle_harbor=debug"));

    let console_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(std::io::stdout().is_terminal())
        .with_level(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .pretty();

    let file_appender = rolling::daily("logs", "turtle_harbor.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false)
        .with_level(true)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .pretty();

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}
