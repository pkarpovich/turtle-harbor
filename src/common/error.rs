use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to read config file {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse config: {0}")]
    ConfigParse(#[from] serde_yml::Error),

    #[error("message size {size} exceeds maximum {max}")]
    MessageTooLarge { size: u32, max: u32 },

    #[error("command timed out")]
    CommandTimeout,

    #[error("script '{name}' not found")]
    ScriptNotFound { name: String },

    #[error("invalid cron expression '{expression}': {source}")]
    CronParse {
        expression: String,
        source: cron::error::Error,
    },

    #[error("no configuration loaded - run 'th up' first")]
    ConfigNotLoaded,

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
