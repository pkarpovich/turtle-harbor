use crate::common::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_MAX_RESTARTS: u32 = 5;
const DEFAULT_MAX_RESTARTS_CRON: u32 = 3;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub settings: Settings,
    pub scripts: HashMap<String, Script>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LokiConfig {
    pub url: String,
    #[serde(default)]
    pub labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NatsSettings {
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub log_dir: PathBuf,
    #[serde(default)]
    pub loki: Option<LokiConfig>,
    #[serde(default)]
    pub nats: Option<NatsSettings>,
}

fn default_ack_wait() -> Duration {
    Duration::from_secs(30 * 60)
}

fn default_max_deliver() -> u32 {
    5
}

fn default_nak_delay() -> Duration {
    Duration::from_secs(5 * 60)
}

fn default_job_timeout() -> Duration {
    Duration::from_secs(60 * 60)
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct NatsTrigger {
    pub stream: String,
    pub subject: String,
    pub durable: String,
    #[serde(with = "humantime_serde", default = "default_ack_wait")]
    pub ack_wait: Duration,
    #[serde(default = "default_max_deliver")]
    pub max_deliver: u32,
    #[serde(with = "humantime_serde", default = "default_nak_delay")]
    pub nak_delay: Duration,
    #[serde(with = "humantime_serde", default = "default_job_timeout")]
    pub job_timeout: Duration,
    #[serde(default)]
    pub publish: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Script {
    pub command: String,
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub max_restarts: Option<u32>,
    pub cron: Option<String>,
    #[serde(default)]
    pub context: Option<PathBuf>,
    #[serde(default)]
    pub venv: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    #[serde(default)]
    pub nats: Option<NatsTrigger>,
}

impl Script {
    pub fn effective_max_restarts(&self) -> u32 {
        self.max_restarts.unwrap_or(if self.cron.is_some() {
            DEFAULT_MAX_RESTARTS_CRON
        } else {
            DEFAULT_MAX_RESTARTS
        })
    }

    pub fn resolved_context(&self, config_dir: &Path) -> Option<PathBuf> {
        self.context.as_ref().map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                config_dir.join(p)
            }
        })
    }

    pub fn resolved_env(&self, config_dir: &Path) -> HashMap<OsString, OsString> {
        let mut env_vars: HashMap<OsString, OsString> = HashMap::new();
        let base_dir = self
            .resolved_context(config_dir)
            .unwrap_or_else(|| config_dir.to_path_buf());

        if let Some(env_file) = &self.env_file {
            let env_file_path = if env_file.is_absolute() {
                env_file.clone()
            } else {
                base_dir.join(env_file)
            };
            let file_vars = parse_env_file(&env_file_path);
            for (k, v) in file_vars {
                env_vars.insert(k.into(), v.into());
            }
        }

        if let Some(venv) = &self.venv {
            let venv_path = if venv.is_absolute() {
                venv.clone()
            } else {
                base_dir.join(venv)
            };
            let venv_bin = venv_path.join("bin");

            let current_path = std::env::var_os("PATH").unwrap_or_default();
            let mut new_path = OsString::from(&venv_bin);
            new_path.push(":");
            new_path.push(&current_path);

            env_vars.insert("PATH".into(), new_path);
            env_vars.insert("VIRTUAL_ENV".into(), venv_path.into_os_string());
        }

        if let Some(user_env) = &self.env {
            for (k, v) in user_env {
                env_vars.insert(k.into(), v.into());
            }
        }

        env_vars
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RestartPolicy {
    Always,
    Never,
}

fn parse_env_file(path: &Path) -> HashMap<String, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to read env file {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    let mut vars = HashMap::new();
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some(eq_pos) = trimmed.find('=') else {
            tracing::warn!(
                "env file {}: skipping malformed line {}: {}",
                path.display(),
                line_num + 1,
                trimmed
            );
            continue;
        };

        let key = trimmed[..eq_pos].trim();
        if key.is_empty() {
            tracing::warn!(
                "env file {}: skipping line {} with empty key",
                path.display(),
                line_num + 1
            );
            continue;
        }

        let raw_value = trimmed[eq_pos + 1..].trim();
        let value = if raw_value.len() >= 2
            && ((raw_value.starts_with('"') && raw_value.ends_with('"'))
                || (raw_value.starts_with('\'') && raw_value.ends_with('\'')))
        {
            &raw_value[1..raw_value.len() - 1]
        } else {
            raw_value
        };

        vars.insert(key.to_string(), value.to_string());
    }

    vars
}

fn trigger_problem(trigger: &NatsTrigger) -> Option<String> {
    let NatsTrigger {
        stream,
        subject,
        durable,
        ack_wait: _,
        max_deliver,
        nak_delay: _,
        job_timeout: _,
        publish,
    } = trigger;

    if stream.trim().is_empty() {
        return Some("stream is empty".to_string());
    }
    if subject.trim().is_empty() {
        return Some("subject is empty".to_string());
    }
    if durable.trim().is_empty() {
        return Some("durable is empty".to_string());
    }
    if *max_deliver == 0 {
        return Some("max_deliver is 0 - JetStream reads that as unlimited".to_string());
    }
    if publish.as_ref().is_some_and(|s| s.trim().is_empty()) {
        return Some("publish is empty".to_string());
    }

    None
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Config = serde_yaml_ng::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, script) in &self.scripts {
            let Some(trigger) = &script.nats else {
                continue;
            };
            if let Some(reason) = trigger_problem(trigger) {
                return Err(Error::InvalidNatsTrigger {
                    name: name.clone(),
                    reason,
                });
            }
            match script.restart_policy {
                RestartPolicy::Always => {
                    return Err(Error::JobRestartPolicy { name: name.clone() })
                }
                RestartPolicy::Never => {}
            }
            if script.cron.is_some() {
                return Err(Error::ConflictingTriggers { name: name.clone() });
            }
            if self.settings.nats.is_none() {
                return Err(Error::NatsUrlMissing { name: name.clone() });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_env_file(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn parse_env_file_basic_key_value() {
        let file = write_env_file("FOO=bar\nBAZ=qux\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("FOO").unwrap(), "bar");
        assert_eq!(vars.get("BAZ").unwrap(), "qux");
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn parse_env_file_comments_and_empty_lines() {
        let file = write_env_file("# this is a comment\n\nFOO=bar\n\n# another comment\nBAZ=qux\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("FOO").unwrap(), "bar");
        assert_eq!(vars.get("BAZ").unwrap(), "qux");
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn parse_env_file_double_quoted_value() {
        let file = write_env_file("SECRET=\"hello world\"\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("SECRET").unwrap(), "hello world");
    }

    #[test]
    fn parse_env_file_single_quoted_value() {
        let file = write_env_file("SECRET='hello world'\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("SECRET").unwrap(), "hello world");
    }

    #[test]
    fn parse_env_file_empty_value() {
        let file = write_env_file("EMPTY_VAR=\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("EMPTY_VAR").unwrap(), "");
    }

    #[test]
    fn parse_env_file_value_with_equals() {
        let file = write_env_file("URL=http://example.com?foo=bar\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("URL").unwrap(), "http://example.com?foo=bar");
    }

    #[test]
    fn parse_env_file_malformed_lines_skipped() {
        let file = write_env_file("GOOD=value\nNO_EQUALS_HERE\nALSO_GOOD=yes\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("GOOD").unwrap(), "value");
        assert_eq!(vars.get("ALSO_GOOD").unwrap(), "yes");
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn parse_env_file_empty_key_skipped() {
        let file = write_env_file("=value\nGOOD=ok\n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("GOOD").unwrap(), "ok");
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn parse_env_file_nonexistent_file_returns_empty() {
        let vars = parse_env_file(Path::new("/nonexistent/path/.env"));
        assert!(vars.is_empty());
    }

    #[test]
    fn parse_env_file_whitespace_around_key_value() {
        let file = write_env_file("  KEY  =  value  \n");
        let vars = parse_env_file(file.path());
        assert_eq!(vars.get("KEY").unwrap(), "value");
    }

    fn make_script_with_env_file(
        env_file: Option<PathBuf>,
        env: Option<HashMap<String, String>>,
    ) -> Script {
        Script {
            command: "echo hello".to_string(),
            restart_policy: RestartPolicy::Never,
            max_restarts: None,
            cron: None,
            context: None,
            venv: None,
            env,
            env_file,
            nats: None,
        }
    }

    fn load_yaml(content: &str) -> Result<Config> {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        Config::load(file.path())
    }

    const NATS_FULL: &str = r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:4222"
scripts:
  transcriber:
    command: "python cli.py --job"
    restart_policy: "never"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
      ack_wait: "30m"
      max_deliver: 7
      nak_delay: "5m"
      job_timeout: "90m"
      publish: "recordings.transcribed"
"#;

    #[test]
    fn nats_block_parses_every_field() {
        let config = load_yaml(NATS_FULL).unwrap();
        assert_eq!(
            config.settings.nats.unwrap().url,
            "nats://127.0.0.1:4222".to_string()
        );

        let nats = config.scripts["transcriber"].nats.clone().unwrap();
        assert_eq!(nats.stream, "recordings");
        assert_eq!(nats.subject, "recordings.completed");
        assert_eq!(nats.durable, "transcriber");
        assert_eq!(nats.ack_wait, Duration::from_secs(1800));
        assert_eq!(nats.max_deliver, 7);
        assert_eq!(nats.nak_delay, Duration::from_secs(300));
        assert_eq!(nats.job_timeout, Duration::from_secs(5400));
        assert_eq!(nats.publish.as_deref(), Some("recordings.transcribed"));
    }

    #[test]
    fn nats_block_optional_fields_take_defaults() {
        let config = load_yaml(
            r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:4222"
scripts:
  transcriber:
    command: "echo job"
    restart_policy: "never"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
"#,
        )
        .unwrap();

        let nats = config.scripts["transcriber"].nats.clone().unwrap();
        assert_eq!(nats.ack_wait, Duration::from_secs(1800));
        assert_eq!(nats.max_deliver, 5);
        assert_eq!(nats.nak_delay, Duration::from_secs(300));
        assert_eq!(nats.job_timeout, Duration::from_secs(3600));
        assert!(nats.publish.is_none());
    }

    #[test]
    fn nats_with_restart_policy_always_rejected() {
        let err = load_yaml(
            r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:4222"
scripts:
  transcriber:
    command: "echo job"
    restart_policy: "always"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
"#,
        )
        .expect_err("expected JobRestartPolicy error");

        match err {
            Error::JobRestartPolicy { name } => assert_eq!(name, "transcriber"),
            other => panic!("expected JobRestartPolicy error, got {}", other),
        }
    }

    #[test]
    fn nats_with_cron_rejected() {
        let err = load_yaml(
            r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:4222"
scripts:
  transcriber:
    command: "echo job"
    restart_policy: "never"
    cron: "0 0 * * * * *"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
"#,
        )
        .expect_err("expected ConflictingTriggers error");

        match err {
            Error::ConflictingTriggers { name } => assert_eq!(name, "transcriber"),
            other => panic!("expected ConflictingTriggers error, got {}", other),
        }
    }

    #[test]
    fn nats_without_settings_nats_rejected() {
        let err = load_yaml(
            r#"
settings:
  log_dir: "./logs"
scripts:
  transcriber:
    command: "echo job"
    restart_policy: "never"
    nats:
      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
"#,
        )
        .expect_err("expected NatsUrlMissing error");

        match err {
            Error::NatsUrlMissing { name } => assert_eq!(name, "transcriber"),
            other => panic!("expected NatsUrlMissing error, got {}", other),
        }
    }

    fn load_trigger_yaml(trigger: &str) -> Error {
        load_yaml(&format!(
            r#"
settings:
  log_dir: "./logs"
  nats:
    url: "nats://127.0.0.1:4222"
scripts:
  transcriber:
    command: "echo job"
    restart_policy: "never"
    nats:
{trigger}
"#
        ))
        .expect_err("expected InvalidNatsTrigger error")
    }

    fn assert_invalid_trigger(trigger: &str, expected_reason: &str) {
        match load_trigger_yaml(trigger) {
            Error::InvalidNatsTrigger { name, reason } => {
                assert_eq!(name, "transcriber");
                assert_eq!(reason, expected_reason);
            }
            other => panic!("expected InvalidNatsTrigger error, got {}", other),
        }
    }

    #[test]
    fn nats_with_empty_stream_rejected() {
        assert_invalid_trigger(
            r#"      stream: ""
      subject: "recordings.completed"
      durable: "transcriber""#,
            "stream is empty",
        );
    }

    #[test]
    fn nats_with_empty_subject_rejected() {
        assert_invalid_trigger(
            r#"      stream: "recordings"
      subject: "   "
      durable: "transcriber""#,
            "subject is empty",
        );
    }

    #[test]
    fn nats_with_empty_durable_rejected() {
        assert_invalid_trigger(
            r#"      stream: "recordings"
      subject: "recordings.completed"
      durable: """#,
            "durable is empty",
        );
    }

    #[test]
    fn nats_with_zero_max_deliver_rejected() {
        assert_invalid_trigger(
            r#"      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
      max_deliver: 0"#,
            "max_deliver is 0 - JetStream reads that as unlimited",
        );
    }

    #[test]
    fn nats_with_empty_publish_rejected() {
        assert_invalid_trigger(
            r#"      stream: "recordings"
      subject: "recordings.completed"
      durable: "transcriber"
      publish: """#,
            "publish is empty",
        );
    }

    #[test]
    fn config_without_nats_loads_unchanged() {
        let config = load_yaml(
            r#"
settings:
  log_dir: "./logs"
scripts:
  worker:
    command: "echo worker"
    restart_policy: "always"
  nightly:
    command: "echo nightly"
    restart_policy: "never"
    cron: "0 0 3 * * * *"
"#,
        )
        .unwrap();

        assert!(config.settings.nats.is_none());
        assert!(config.scripts["worker"].nats.is_none());
        assert_eq!(
            config.scripts["worker"].restart_policy,
            RestartPolicy::Always
        );
        assert_eq!(
            config.scripts["nightly"].cron.as_deref(),
            Some("0 0 3 * * * *")
        );
    }

    #[test]
    fn resolved_env_loads_env_file_vars() {
        let file = write_env_file("API_KEY=secret123\nDB_HOST=localhost\n");
        let script = make_script_with_env_file(Some(file.path().to_path_buf()), None);
        let env = script.resolved_env(Path::new("/tmp"));
        assert_eq!(env.get(&OsString::from("API_KEY")).unwrap(), "secret123");
        assert_eq!(env.get(&OsString::from("DB_HOST")).unwrap(), "localhost");
    }

    #[test]
    fn resolved_env_inline_overrides_env_file() {
        let file = write_env_file("SHARED=from_file\nFILE_ONLY=file_val\n");
        let mut inline_env = HashMap::new();
        inline_env.insert("SHARED".to_string(), "from_inline".to_string());
        inline_env.insert("INLINE_ONLY".to_string(), "inline_val".to_string());

        let script = make_script_with_env_file(Some(file.path().to_path_buf()), Some(inline_env));
        let env = script.resolved_env(Path::new("/tmp"));

        assert_eq!(env.get(&OsString::from("SHARED")).unwrap(), "from_inline");
        assert_eq!(env.get(&OsString::from("FILE_ONLY")).unwrap(), "file_val");
        assert_eq!(
            env.get(&OsString::from("INLINE_ONLY")).unwrap(),
            "inline_val"
        );
    }

    #[test]
    fn resolved_env_missing_env_file_continues() {
        let script = make_script_with_env_file(Some(PathBuf::from("/nonexistent/.env")), None);
        let env = script.resolved_env(Path::new("/tmp"));
        assert!(env.is_empty());
    }

    #[test]
    fn resolved_env_no_env_file_returns_only_inline() {
        let mut inline_env = HashMap::new();
        inline_env.insert("FOO".to_string(), "bar".to_string());
        let script = make_script_with_env_file(None, Some(inline_env));
        let env = script.resolved_env(Path::new("/tmp"));
        assert_eq!(env.get(&OsString::from("FOO")).unwrap(), "bar");
        assert_eq!(env.len(), 1);
    }
}
