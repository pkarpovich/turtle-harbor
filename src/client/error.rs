use crate::common::error::Error;
use std::process;

pub fn handle_error(error: Error) -> ! {
    let message = match &error {
        Error::Io(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            "Daemon is not running. Please start turtled first".to_string()
        }
        Error::Io(e) => format!("IO error: {}", e),
        Error::ConfigRead { path, source } => {
            format!("Failed to read config {}: {}", path.display(), source)
        }
        Error::ConfigParse(e) => format!("Configuration error: {}", e),
        Error::MessageTooLarge { size, max } => {
            format!("Message too large: {} bytes (max {})", size, max)
        }
        Error::CommandTimeout => "Command timed out waiting for daemon response".to_string(),
        Error::ScriptNotFound { name } => format!("Script '{}' not found", name),
        Error::ConfigNotLoaded => "No configuration loaded - run 'th up' first".to_string(),
        Error::CronParse {
            expression,
            source,
        } => format!("Invalid cron '{}': {}", expression, source),
        Error::Json(e) => format!("JSON error: {}", e),
    };

    eprintln!("{}", message);
    process::exit(1);
}
