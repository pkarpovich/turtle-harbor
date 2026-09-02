use crate::common::config::NatsTrigger;
use crate::common::error::{Error, Result};
use crate::daemon::daemon_core::DaemonEvent;
use async_nats::header::HeaderMap;
use async_nats::jetstream::consumer::{pull, AckPolicy, DeliverPolicy};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

const TRACEPARENT_HEADER: &str = "traceparent";
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Listener {
    handle: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

pub struct NatsManager {
    tasks: HashMap<String, Listener>,
    event_tx: mpsc::Sender<DaemonEvent>,
}

impl NatsManager {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self {
            tasks: HashMap::new(),
            event_tx,
        }
    }

    pub async fn listen(&mut self, name: &str, trigger: &NatsTrigger, url: &str) {
        self.cancel(name).await;

        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let name = name.to_string();
        let subject = trigger.subject.clone();
        let url = url.to_string();

        let task_name = name.clone();
        let handle = tokio::spawn(async move {
            tracing::debug!(script = %task_name, url = %url, subject = %subject, "Listener started");
            loop {
                let stop = *shutdown_rx.borrow_and_update();
                if stop {
                    break;
                }
                let Ok(()) = shutdown_rx.changed().await else {
                    break;
                };
            }
            tracing::debug!(script = %task_name, "Listener stopped");
        });

        self.tasks
            .insert(name.clone(), Listener { handle, shutdown });
        tracing::info!(script = %name, "Listener registered");
    }

    pub async fn cancel(&mut self, name: &str) {
        let Some(Listener { handle, shutdown }) = self.tasks.remove(name) else {
            return;
        };

        let _ = shutdown.send(true);
        let abort = handle.abort_handle();
        match tokio::time::timeout(CANCEL_TIMEOUT, handle).await {
            Ok(_) => {
                tracing::debug!(script = %name, "Listener cancelled");
            }
            Err(_) => {
                abort.abort();
                tracing::warn!(script = %name, "Listener did not stop in time, aborted");
            }
        }
    }

    pub async fn cancel_all(&mut self) {
        let mut stopping = Vec::new();
        for (name, listener) in self.tasks.drain() {
            let Listener { handle, shutdown } = listener;
            let _ = shutdown.send(true);
            stopping.push((name, handle));
        }

        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        for (name, handle) in stopping {
            let abort = handle.abort_handle();
            match tokio::time::timeout_at(deadline, handle).await {
                Ok(_) => {
                    tracing::debug!(script = %name, "Listener cancelled");
                }
                Err(_) => {
                    abort.abort();
                    tracing::warn!(script = %name, "Listener did not stop in time, aborted");
                }
            }
        }
    }

    pub fn is_listening(&self, name: &str) -> bool {
        self.tasks.contains_key(name)
    }
}

pub fn consumer_config(trigger: &NatsTrigger) -> pull::Config {
    let NatsTrigger {
        stream: _,
        subject,
        durable,
        ack_wait,
        max_deliver,
        nak_delay: _,
        job_timeout: _,
        publish: _,
    } = trigger;

    pull::Config {
        durable_name: Some(durable.clone()),
        name: Some(durable.clone()),
        filter_subject: subject.clone(),
        ack_wait: *ack_wait,
        max_deliver: i64::from(*max_deliver),
        deliver_policy: DeliverPolicy::New,
        ack_policy: AckPolicy::Explicit,
        ..Default::default()
    }
}

pub fn traceparent(headers: Option<&HeaderMap>) -> Option<String> {
    let headers = headers?;
    let value = headers.get(TRACEPARENT_HEADER)?;
    Some(value.as_str().to_string())
}

pub fn publish_headers(traceparent: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(traceparent) = traceparent {
        headers.insert(TRACEPARENT_HEADER, traceparent);
    }
    headers
}

pub fn read_result(path: &Path) -> Result<Vec<u8>> {
    let Ok(bytes) = std::fs::read(path) else {
        return Err(Error::JobResultMissing {
            path: path.to_path_buf(),
        });
    };

    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(_) => Ok(bytes),
        Err(source) => Err(Error::JobResultInvalid {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::paths;
    use tempfile::TempDir;

    const DEAD_URL: &str = "nats://127.0.0.1:1";

    fn make_trigger() -> NatsTrigger {
        NatsTrigger {
            stream: "recordings".to_string(),
            subject: "recordings.completed".to_string(),
            durable: "podcast-transcriber".to_string(),
            ack_wait: Duration::from_secs(1800),
            max_deliver: 7,
            nak_delay: Duration::from_secs(300),
            job_timeout: Duration::from_secs(5400),
            publish: Some("recordings.transcribed".to_string()),
        }
    }

    fn make_manager() -> NatsManager {
        let (event_tx, _event_rx) = mpsc::channel(16);
        NatsManager::new(event_tx)
    }

    #[test]
    fn test_consumer_config_maps_every_field() {
        let config = consumer_config(&make_trigger());

        assert_eq!(config.durable_name, Some("podcast-transcriber".to_string()));
        assert_eq!(config.name, Some("podcast-transcriber".to_string()));
        assert_eq!(config.filter_subject, "recordings.completed");
        assert_eq!(config.ack_wait, Duration::from_secs(1800));
        assert_eq!(config.max_deliver, 7);
        assert_eq!(config.deliver_policy, DeliverPolicy::New);
        assert_eq!(config.ack_policy, AckPolicy::Explicit);
    }

    #[test]
    fn test_traceparent_present() {
        let mut headers = HeaderMap::new();
        headers.insert(TRACEPARENT_HEADER, "00-abc-def-01");

        assert_eq!(
            traceparent(Some(&headers)),
            Some("00-abc-def-01".to_string())
        );
    }

    #[test]
    fn test_traceparent_absent() {
        let headers = HeaderMap::new();

        assert_eq!(traceparent(Some(&headers)), None);
        assert_eq!(traceparent(None), None);
    }

    #[test]
    fn test_publish_headers_carry_traceparent() {
        let headers = publish_headers(Some("00-abc-def-01"));
        assert_eq!(
            traceparent(Some(&headers)),
            Some("00-abc-def-01".to_string())
        );

        let headers = publish_headers(None);
        assert_eq!(traceparent(Some(&headers)), None);
    }

    #[test]
    fn test_read_result_accepts_json_object() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.result.json");
        std::fs::write(&path, br#"{"ok":true}"#).unwrap();

        assert_eq!(read_result(&path).unwrap(), br#"{"ok":true}"#.to_vec());
    }

    #[test]
    fn test_read_result_rejects_absent_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.result.json");

        match read_result(&path) {
            Err(Error::JobResultMissing { path: reported }) => assert_eq!(reported, path),
            other => panic!("expected JobResultMissing, got {other:?}"),
        }
    }

    #[test]
    fn test_read_result_rejects_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.result.json");
        std::fs::write(&path, b"").unwrap();

        match read_result(&path) {
            Err(Error::JobResultInvalid { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("expected JobResultInvalid, got {other:?}"),
        }
    }

    #[test]
    fn test_read_result_rejects_non_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.result.json");
        std::fs::write(&path, b"not json").unwrap();

        match read_result(&path) {
            Err(Error::JobResultInvalid { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("expected JobResultInvalid, got {other:?}"),
        }
    }

    #[test]
    fn test_result_path_under_jobs_dir() {
        let path = paths::result_path("x");
        assert!(path.ends_with("x.result.json"));
        assert_eq!(path.parent(), Some(paths::jobs_dir().as_path()));
    }

    #[tokio::test]
    async fn test_listen_registers_and_cancel_removes() {
        let mut manager = make_manager();
        let trigger = make_trigger();

        manager.listen("job", &trigger, DEAD_URL).await;
        assert!(manager.is_listening("job"));

        manager.cancel("job").await;
        assert!(!manager.is_listening("job"));
    }

    #[tokio::test]
    async fn test_cancel_unknown_name_is_noop() {
        let mut manager = make_manager();

        manager.cancel("nobody").await;
        assert!(!manager.is_listening("nobody"));
    }

    #[tokio::test]
    async fn test_cancel_all_stops_every_listener() {
        let mut manager = make_manager();
        let trigger = make_trigger();

        manager.listen("first", &trigger, DEAD_URL).await;
        manager.listen("second", &trigger, DEAD_URL).await;
        assert!(manager.is_listening("first"));
        assert!(manager.is_listening("second"));

        manager.cancel_all().await;
        assert!(!manager.is_listening("first"));
        assert!(!manager.is_listening("second"));
    }

    #[tokio::test]
    async fn test_listen_twice_replaces_the_listener() {
        let mut manager = make_manager();
        let trigger = make_trigger();

        manager.listen("job", &trigger, DEAD_URL).await;
        manager.listen("job", &trigger, DEAD_URL).await;

        assert!(manager.is_listening("job"));
        manager.cancel_all().await;
        assert!(!manager.is_listening("job"));
    }
}
