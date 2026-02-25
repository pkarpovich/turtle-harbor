use crate::common::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ffi::OsString;

const DEFAULT_MAX_RESTARTS: u32 = 5;
const DEFAULT_MAX_RESTARTS_CRON: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    pub log_dir: PathBuf,
    #[serde(default)]
    pub loki: Option<LokiConfig>,
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
        let base_dir = self.resolved_context(config_dir).unwrap_or_else(|| config_dir.to_path_buf());

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
            tracing::warn!("env file {}: skipping malformed line {}: {}", path.display(), line_num + 1, trimmed);
            continue;
        };

        let key = trimmed[..eq_pos].trim();
        if key.is_empty() {
            tracing::warn!("env file {}: skipping line {} with empty key", path.display(), line_num + 1);
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

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let config = serde_yml::from_str(&content)?;
        Ok(config)
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

    fn make_script_with_env_file(env_file: Option<PathBuf>, env: Option<HashMap<String, String>>) -> Script {
        Script {
            command: "echo hello".to_string(),
            restart_policy: RestartPolicy::Never,
            max_restarts: None,
            cron: None,
            context: None,
            venv: None,
            env,
            env_file,
        }
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
        assert_eq!(env.get(&OsString::from("INLINE_ONLY")).unwrap(), "inline_val");
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
