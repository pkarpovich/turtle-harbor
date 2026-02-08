use crate::common::error::Error;
use std::process;

pub fn handle_error(error: Error) -> ! {
    let message = match &error {
        Error::Ipc(msg) => format!("Daemon error: {}", msg),
        Error::Io(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            "Daemon is not running. Please start turtled first".to_string()
        }
        Error::Process(msg) => format!("Process error: {}", msg),
        Error::Config(msg) => format!("Configuration error: {}", msg),
        Error::Json(e) => format!("JSON error: {}", e),
        Error::Yaml(e) => format!("YAML error: {}", e),
        Error::Io(e) => format!("IO error: {}", e),
        Error::Other(msg) => msg.to_string(),
    };

    eprintln!("{}", message);
    process::exit(1);
}
