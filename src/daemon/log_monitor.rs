use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};

use crate::common::error::Result;

const MAX_LINE_LENGTH: usize = 8192;
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
const MAX_ROTATED_FILES: u32 = 5;
const FLUSH_INTERVAL_MS: u64 = 500;
const CHANNEL_BUFFER: usize = 1024;

pub enum LogLine {
    Stdout(String),
    Stderr(String),
}

pub struct ScriptLogger {
    pub tx: mpsc::Sender<LogLine>,
    writer_handle: JoinHandle<()>,
}

impl ScriptLogger {
    pub fn new(log_path: PathBuf, broadcast_tx: broadcast::Sender<String>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<LogLine>(CHANNEL_BUFFER);

        let writer_handle = tokio::spawn(async move {
            if let Err(e) = writer_loop(rx, &log_path, broadcast_tx).await {
                tracing::error!(error = ?e, path = ?log_path, "Log writer error");
            }
        });

        Ok(Self { tx, writer_handle })
    }

    pub fn shutdown(self) {
        drop(self.tx);
        self.writer_handle.abort();
    }
}

struct RotatingWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    bytes_written: u64,
}

impl RotatingWriter {
    fn new(path: &Path) -> std::io::Result<Self> {
        let bytes_written = if path.exists() {
            std::fs::metadata(path)?.len()
        } else {
            0
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        Ok(Self {
            path: path.to_path_buf(),
            writer: BufWriter::with_capacity(8192, file),
            bytes_written,
        })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = line.as_bytes();
        self.writer.write_all(bytes)?;
        self.bytes_written += bytes.len() as u64;

        if self.bytes_written >= MAX_FILE_SIZE {
            self.rotate()?;
        }

        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;

        for i in (1..MAX_ROTATED_FILES).rev() {
            let from = rotated_path(&self.path, i);
            let to = rotated_path(&self.path, i + 1);
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }

        let first_rotated = rotated_path(&self.path, 1);
        std::fs::rename(&self.path, &first_rotated)?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.writer = BufWriter::with_capacity(8192, file);
        self.bytes_written = 0;

        tracing::debug!(path = ?self.path, "Log file rotated");
        Ok(())
    }
}

fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let stem = base.file_stem().unwrap_or_default().to_string_lossy();
    let parent = base.parent().unwrap_or(Path::new("."));
    parent.join(format!("{}.log.{}", stem, index))
}

async fn writer_loop(
    mut rx: mpsc::Receiver<LogLine>,
    log_path: &Path,
    broadcast_tx: broadcast::Sender<String>,
) -> std::io::Result<()> {
    let mut writer = RotatingWriter::new(log_path)?;
    let mut flush_interval = interval(Duration::from_millis(FLUSH_INTERVAL_MS));

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(line) => {
                        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                        let (stream, content) = match &line {
                            LogLine::Stdout(s) => ("stdout", s.as_str()),
                            LogLine::Stderr(s) => ("stderr", s.as_str()),
                        };
                        let content = if content.len() > MAX_LINE_LENGTH {
                            &content[..MAX_LINE_LENGTH]
                        } else {
                            content
                        };
                        let formatted = format!("[{}] [{}] {}\n", timestamp, stream, content);
                        if let Err(e) = writer.write_line(&formatted) {
                            tracing::error!(error = ?e, "Failed to write log line");
                        }
                        let _ = broadcast_tx.send(formatted);
                    }
                    None => {
                        let _ = writer.flush();
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                if let Err(e) = writer.flush() {
                    tracing::error!(error = ?e, "Failed to flush log file");
                }
            }
        }
    }

    Ok(())
}

pub fn spawn_stdout_reader<R>(reader: BufReader<R>, tx: mpsc::Sender<LogLine>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(LogLine::Stdout(line.trim_end().to_string())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "Error reading stdout");
                    break;
                }
            }
        }
    });
}

pub fn spawn_stderr_reader<R>(reader: BufReader<R>, tx: mpsc::Sender<LogLine>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(LogLine::Stderr(line.trim_end().to_string())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "Error reading stderr");
                    break;
                }
            }
        }
    });
}

pub fn read_last_n_lines(path: &Path, n: u32) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    if file_size == 0 {
        return Ok(String::new());
    }

    let n = n as usize;
    let mut chunk_size: u64 = 8192;
    let mut collected = Vec::new();
    let mut remaining = file_size;

    loop {
        let read_size = chunk_size.min(remaining);
        remaining -= read_size;
        file.seek(SeekFrom::Start(remaining))?;

        let mut buf = vec![0u8; read_size as usize];
        file.read_exact(&mut buf)?;

        collected.splice(0..0, buf);

        let newline_count = collected.iter().filter(|&&b| b == b'\n').count();
        let has_trailing_newline = collected.last() == Some(&b'\n');
        let line_count = if has_trailing_newline { newline_count } else { newline_count + 1 };

        if line_count > n || remaining == 0 {
            break;
        }

        chunk_size *= 2;
    }

    let text = String::from_utf8_lossy(&collected);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].join("\n"))
}

pub fn ensure_log_dir(log_dir: &Path) -> Result<()> {
    if !log_dir.exists() {
        tracing::debug!(path = ?log_dir, "Creating log directory");
        std::fs::create_dir_all(log_dir)?;
    }
    Ok(())
}

pub fn get_log_path(log_dir: &Path, name: &str) -> PathBuf {
    log_dir.join(format!("{}.log", name))
}

pub fn list_script_names(log_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".log").map(|s| s.to_string())
        })
        .filter(|name| !name.contains('.'))
        .collect();
    names.sort();
    names
}
