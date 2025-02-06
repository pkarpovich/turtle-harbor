use chrono::Local;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use crate::common::error::Result;

pub async fn spawn_log_reader<R>(mut reader: BufReader<R>, file: Arc<Mutex<std::fs::File>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
                    let log_line = format!("[{}] {}\n", timestamp, line.trim());
                    let mut f = file.lock().await;
                    if let Err(e) = f.write_all(log_line.as_bytes()) {
                        tracing::error!(error = ?e, "Failed to write to log file");
                    }
                    line.clear();
                }
                Err(e) => {
                    tracing::error!(error = ?e, "Error reading process output");
                    break;
                }
            }
        }
    });
}

pub fn ensure_log_dir() -> Result<()> {
    let log_dir = PathBuf::from("logs");
    if !log_dir.exists() {
        tracing::debug!(path = ?log_dir, "Creating log directory");
        std::fs::create_dir_all(&log_dir)?;
    }
    Ok(())
}

pub fn get_log_path(name: &str) -> PathBuf {
    let path = PathBuf::from("logs").join(format!("{}.log", name));
    tracing::trace!(script = %name, path = ?path, "Generated log path");
    path
}
