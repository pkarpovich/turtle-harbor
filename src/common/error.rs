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
    ConfigParse(#[from] serde_yaml_ng::Error),

    #[error("message size {size} exceeds maximum {max}")]
    MessageTooLarge { size: u32, max: u32 },

    #[error("command timed out")]
    CommandTimeout,

    #[error("script '{name}' not found")]
    ScriptNotFound { name: String },

    #[error("script '{name}' already registered from '{path}'")]
    DuplicateScript { name: String, path: PathBuf },

    #[error("invalid cron expression '{expression}': {source}")]
    CronParse {
        expression: String,
        source: cron::error::Error,
    },

    #[error("no configuration loaded - run 'th up' first")]
    ConfigNotLoaded,

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("script '{name}' has a nats trigger and restart_policy 'always'")]
    JobRestartPolicy { name: String },

    #[error("script '{name}' has both a nats trigger and a cron schedule")]
    ConflictingTriggers { name: String },

    #[error("script '{name}' has a nats trigger but settings.nats is missing")]
    NatsUrlMissing { name: String },

    #[error("durable '{durable}' on stream '{stream}' already registered from '{path}'")]
    DuplicateDurable {
        stream: String,
        durable: String,
        path: PathBuf,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
