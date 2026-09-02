use crate::common::config::NatsTrigger;
use crate::common::error::{Error, Result};
use crate::common::paths;
use crate::daemon::daemon_core::DaemonEvent;
use crate::daemon::job::{self, Delivery, JobInput, JobOutcome, Verdict};
use async_nats::header::HeaderMap;
use async_nats::jetstream::consumer::{pull, AckPolicy, Consumer, DeliverPolicy};
use async_nats::jetstream::{self, AckKind, Context, Message};
use async_nats::ConnectOptions;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const TRACEPARENT_HEADER: &str = "traceparent";
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_secs(30);
const BATCH_EXPIRES: Duration = Duration::from_secs(60);
const BATCH_HEARTBEAT: Duration = Duration::from_secs(20);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

pub struct Listener {
    handle: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
struct ListenerConfig {
    name: String,
    trigger: NatsTrigger,
    url: String,
    event_tx: mpsc::Sender<DaemonEvent>,
}

enum SessionExit {
    Shutdown,
    Failed(String),
}

enum AfterMessage {
    Continue,
    Stop,
}

enum JobResolution {
    Outcome(JobOutcome),
    Shutdown,
}

enum PublishFailure {
    Contract(String),
    Transient(String),
}

enum ResultFile {
    NotNeeded,
    Created(PathBuf),
    Failed,
}

struct Attempt {
    input: JobInput,
    stream_sequence: u64,
}

struct Session {
    config: ListenerConfig,
    context: Context,
    consumer: Consumer<pull::Config>,
    max_deliver: u32,
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

        let (shutdown, shutdown_rx) = watch::channel(false);
        let config = ListenerConfig {
            name: name.to_string(),
            trigger: trigger.clone(),
            url: url.to_string(),
            event_tx: self.event_tx.clone(),
        };

        let handle = tokio::spawn(run_listener(config, shutdown_rx));

        self.tasks
            .insert(name.to_string(), Listener { handle, shutdown });
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

pub fn delivered_count(delivered: i64) -> u32 {
    if delivered <= 0 {
        return 1;
    }
    u32::try_from(delivered).unwrap_or(u32::MAX)
}

pub fn server_max_deliver(max_deliver: i64) -> u32 {
    if max_deliver <= 0 {
        return u32::MAX;
    }
    u32::try_from(max_deliver).unwrap_or(u32::MAX)
}

pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((_, host)) = rest.rsplit_once('@') else {
        return url.to_string();
    };
    format!("{scheme}://***@{host}")
}

async fn run_listener(config: ListenerConfig, mut shutdown: watch::Receiver<bool>) {
    tracing::debug!(script = %config.name, url = %redact_url(&config.url), subject = %config.trigger.subject, "Listener started");

    loop {
        if *shutdown.borrow() {
            break;
        }

        match run_session(&config, &mut shutdown).await {
            SessionExit::Shutdown => break,
            SessionExit::Failed(error) => {
                tracing::warn!(script = %config.name, %error, "NATS listener failed, retrying");
                let sent = config
                    .event_tx
                    .send(DaemonEvent::ListenerFailed {
                        name: config.name.clone(),
                        error,
                    })
                    .await;
                let Ok(()) = sent else {
                    break;
                };
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(RETRY_DELAY) => {}
        }
    }

    tracing::debug!(script = %config.name, "Listener stopped");
}

async fn run_session(config: &ListenerConfig, shutdown: &mut watch::Receiver<bool>) -> SessionExit {
    let bound = tokio::select! {
        biased;
        _ = shutdown.changed() => return SessionExit::Shutdown,
        bound = bind(config) => bound,
    };

    let session = match bound {
        Ok(session) => session,
        Err(error) => return SessionExit::Failed(error),
    };

    let sent = config
        .event_tx
        .send(DaemonEvent::ListenerReady {
            name: config.name.clone(),
        })
        .await;
    let Ok(()) = sent else {
        return SessionExit::Shutdown;
    };

    loop {
        let message = tokio::select! {
            biased;
            _ = shutdown.changed() => return SessionExit::Shutdown,
            message = session.next_message() => message,
        };

        let message = match message {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => return SessionExit::Failed(error),
        };

        match session.handle_message(message, shutdown).await {
            AfterMessage::Continue => {}
            AfterMessage::Stop => return SessionExit::Shutdown,
        }
    }
}

async fn bind(config: &ListenerConfig) -> std::result::Result<Session, String> {
    let ListenerConfig {
        name,
        trigger,
        url,
        event_tx: _,
    } = config;

    let client = ConnectOptions::new().connect(url.as_str()).await;
    let client = match client {
        Ok(client) => client,
        Err(e) => return Err(format!("connect to {}: {e}", redact_url(url))),
    };

    let context = jetstream::new(client);
    let stream = match context.get_stream(&trigger.stream).await {
        Ok(stream) => stream,
        Err(e) => return Err(format!("stream '{}': {e}", trigger.stream)),
    };

    let consumer = stream
        .get_or_create_consumer(&trigger.durable, consumer_config(trigger))
        .await;
    let consumer = match consumer {
        Ok(consumer) => consumer,
        Err(e) => return Err(format!("consumer '{}': {e}", trigger.durable)),
    };

    let server = &consumer.cached_info().config;
    let max_deliver = server_max_deliver(server.max_deliver);
    if max_deliver != trigger.max_deliver || server.ack_wait != trigger.ack_wait {
        tracing::warn!(
            script = %name,
            server_max_deliver = max_deliver,
            config_max_deliver = trigger.max_deliver,
            server_ack_wait = ?server.ack_wait,
            config_ack_wait = ?trigger.ack_wait,
            "Durable exists with different settings - server values apply"
        );
    }

    Ok(Session {
        config: config.clone(),
        context,
        consumer,
        max_deliver,
    })
}

impl Session {
    async fn next_message(&self) -> std::result::Result<Option<Message>, String> {
        let batch = self
            .consumer
            .batch()
            .max_messages(1)
            .expires(BATCH_EXPIRES)
            .heartbeat(BATCH_HEARTBEAT)
            .messages()
            .await;

        let mut batch = match batch {
            Ok(batch) => batch,
            Err(e) => return Err(format!("pull request: {e}")),
        };

        match batch.next().await {
            None => Ok(None),
            Some(Ok(message)) => Ok(Some(message)),
            Some(Err(e)) => Err(format!("fetch: {e}")),
        }
    }

    async fn handle_message(
        &self,
        message: Message,
        shutdown: &mut watch::Receiver<bool>,
    ) -> AfterMessage {
        let name = self.config.name.as_str();
        let subject = message.subject.to_string();

        let Ok(info) = message.info() else {
            tracing::error!(script = %name, %subject, "Message carries no JetStream metadata");
            self.settle(&message, Verdict::Nak).await;
            return AfterMessage::Continue;
        };
        let delivery = Delivery {
            delivered: delivered_count(info.delivered),
            max_deliver: self.max_deliver,
        };
        let stream_sequence = info.stream_sequence;

        let Ok(payload) = String::from_utf8(message.payload.to_vec()) else {
            tracing::error!(script = %name, %subject, stream_sequence, "Payload is not UTF-8, terminating message");
            self.settle(&message, Verdict::Term).await;
            return AfterMessage::Continue;
        };

        let result_path = match self.prepare_result_file() {
            ResultFile::NotNeeded => None,
            ResultFile::Created(path) => Some(path),
            ResultFile::Failed => {
                tracing::error!(script = %name, %subject, stream_sequence, "Cannot create the job result file, retrying the message without running the job");
                self.settle(&message, Verdict::Nak).await;
                return AfterMessage::Continue;
            }
        };

        let input = JobInput {
            payload,
            subject,
            delivery,
            traceparent: traceparent(message.headers.as_ref()),
            result_path,
        };
        let env = job::job_env(&input);
        let attempt = Attempt {
            input,
            stream_sequence,
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = self
            .config
            .event_tx
            .send(DaemonEvent::JobTrigger {
                name: self.config.name.clone(),
                env,
                reply_tx,
            })
            .await;
        let Ok(()) = sent else {
            self.remove_result_file(&attempt);
            return AfterMessage::Stop;
        };

        let outcome = match self.await_outcome(&message, reply_rx, shutdown).await {
            JobResolution::Outcome(outcome) => outcome,
            JobResolution::Shutdown => {
                let nak = message
                    .double_ack_with(AckKind::Nak(Some(Duration::ZERO)))
                    .await;
                if let Err(e) = nak {
                    tracing::error!(script = %name, error = %e, "Failed to nak in-flight message on shutdown");
                }
                self.remove_result_file(&attempt);
                return AfterMessage::Stop;
            }
        };

        let verdict = self.decide(&attempt, outcome.clone()).await;
        match verdict {
            Verdict::Term => tracing::error!(
                script = %name,
                subject = %attempt.input.subject,
                stream_sequence = attempt.stream_sequence,
                delivered = attempt.input.delivery.delivered,
                max_deliver = attempt.input.delivery.max_deliver,
                ?outcome,
                "Terminating message - it will not be redelivered"
            ),
            Verdict::Ack | Verdict::Nak => {}
        }
        self.settle(&message, verdict).await;
        self.remove_result_file(&attempt);

        tracing::info!(
            script = %name,
            subject = %attempt.input.subject,
            stream_sequence = attempt.stream_sequence,
            delivered = attempt.input.delivery.delivered,
            ?outcome,
            ?verdict,
            "Job attempt settled"
        );

        AfterMessage::Continue
    }

    async fn await_outcome(
        &self,
        message: &Message,
        reply_rx: oneshot::Receiver<JobOutcome>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> JobResolution {
        let name = self.config.name.as_str();
        let mut reply_rx = reply_rx;
        let mut progress = tokio::time::interval_at(
            tokio::time::Instant::now() + PROGRESS_INTERVAL,
            PROGRESS_INTERVAL,
        );
        let timeout = tokio::time::sleep(self.config.trigger.job_timeout);
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                biased;
                reply = &mut reply_rx => {
                    return match reply {
                        Ok(outcome) => JobResolution::Outcome(outcome),
                        Err(e) => {
                            tracing::error!(script = %name, error = %e, "Job reply channel closed without an outcome");
                            JobResolution::Outcome(JobOutcome::ReplyLost)
                        }
                    };
                }
                _ = shutdown.changed() => return JobResolution::Shutdown,
                _ = &mut timeout => {
                    return self.resolve_timeout(reply_rx, shutdown).await;
                }
                _ = progress.tick() => {
                    if let Err(e) = message.ack_with(AckKind::Progress).await {
                        tracing::warn!(script = %name, error = %e, "Failed to send job heartbeat");
                    }
                }
            }
        }
    }

    async fn resolve_timeout(
        &self,
        reply_rx: oneshot::Receiver<JobOutcome>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> JobResolution {
        let name = self.config.name.as_str();
        tracing::error!(script = %name, timeout_secs = self.config.trigger.job_timeout.as_secs(), "Job exceeded job_timeout");

        let sent = self
            .config
            .event_tx
            .send(DaemonEvent::JobTimeout {
                name: self.config.name.clone(),
            })
            .await;
        let Ok(()) = sent else {
            return JobResolution::Outcome(JobOutcome::TimedOut);
        };

        tokio::select! {
            biased;
            reply = reply_rx => match reply {
                Ok(outcome) => JobResolution::Outcome(outcome),
                Err(_) => JobResolution::Outcome(JobOutcome::TimedOut),
            },
            _ = shutdown.changed() => JobResolution::Shutdown,
            _ = tokio::time::sleep(TIMEOUT_GRACE) => JobResolution::Outcome(JobOutcome::TimedOut),
        }
    }

    async fn decide(&self, attempt: &Attempt, outcome: JobOutcome) -> Verdict {
        let verdict = job::verdict(outcome, attempt.input.delivery);
        match verdict {
            Verdict::Nak => return Verdict::Nak,
            Verdict::Term => return Verdict::Term,
            Verdict::Ack => {}
        }

        let Some(subject) = self.config.trigger.publish.clone() else {
            return Verdict::Ack;
        };

        match self.publish_result(&subject, attempt).await {
            Ok(()) => Verdict::Ack,
            Err(PublishFailure::Contract(reason)) => {
                tracing::error!(
                    script = %self.config.name,
                    subject = %attempt.input.subject,
                    stream_sequence = attempt.stream_sequence,
                    %reason,
                    "Job result file violates the contract, terminating message"
                );
                Verdict::Term
            }
            Err(PublishFailure::Transient(reason)) => {
                tracing::error!(
                    script = %self.config.name,
                    publish_subject = %subject,
                    stream_sequence = attempt.stream_sequence,
                    %reason,
                    "Failed to publish job result"
                );
                job::verdict(JobOutcome::PublishFailed, attempt.input.delivery)
            }
        }
    }

    async fn publish_result(
        &self,
        subject: &str,
        attempt: &Attempt,
    ) -> std::result::Result<(), PublishFailure> {
        let Some(path) = &attempt.input.result_path else {
            return Err(PublishFailure::Transient(
                "result file was never created".to_string(),
            ));
        };

        let payload = match read_result(path) {
            Ok(payload) => payload,
            Err(e) => return Err(PublishFailure::Contract(e.to_string())),
        };

        let headers = publish_headers(attempt.input.traceparent.as_deref());
        let ack = self
            .context
            .publish_with_headers(subject.to_string(), headers, payload.into())
            .await;
        let ack = match ack {
            Ok(ack) => ack,
            Err(e) => return Err(PublishFailure::Transient(e.to_string())),
        };

        match ack.await {
            Ok(_) => Ok(()),
            Err(e) => Err(PublishFailure::Transient(e.to_string())),
        }
    }

    async fn settle(&self, message: &Message, verdict: Verdict) {
        let kind = match verdict {
            Verdict::Ack => AckKind::Ack,
            Verdict::Nak => AckKind::Nak(Some(self.config.trigger.nak_delay)),
            Verdict::Term => AckKind::Term,
        };

        if let Err(e) = message.ack_with(kind).await {
            tracing::error!(script = %self.config.name, error = %e, ?verdict, "Failed to settle message");
        }
    }

    fn prepare_result_file(&self) -> ResultFile {
        if self.config.trigger.publish.is_none() {
            return ResultFile::NotNeeded;
        }

        let dir = paths::jobs_dir();
        if let Err(e) = paths::ensure_dir(&dir) {
            tracing::error!(script = %self.config.name, error = %e, "Failed to create jobs directory");
            return ResultFile::Failed;
        }

        let path = paths::result_path(&self.config.name);
        if let Err(e) = std::fs::write(&path, b"") {
            tracing::error!(script = %self.config.name, error = %e, "Failed to create job result file");
            return ResultFile::Failed;
        }

        ResultFile::Created(path)
    }

    fn remove_result_file(&self, attempt: &Attempt) {
        let Some(path) = &attempt.input.result_path else {
            return;
        };
        if let Err(e) = std::fs::remove_file(path) {
            tracing::debug!(script = %self.config.name, error = %e, "Job result file already gone");
        }
    }
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
    fn test_redact_url_strips_credentials() {
        assert_eq!(
            redact_url("nats://user:s3cret@nats.example.com:4222"),
            "nats://***@nats.example.com:4222"
        );
        assert_eq!(
            redact_url("nats://token@nats.example.com:4222"),
            "nats://***@nats.example.com:4222"
        );
        assert_eq!(
            redact_url("nats://nats.example.com:4222"),
            "nats://nats.example.com:4222"
        );
        assert_eq!(redact_url("not a url"), "not a url");
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
    fn test_delivered_count_never_below_one() {
        assert_eq!(delivered_count(0), 1);
        assert_eq!(delivered_count(-3), 1);
        assert_eq!(delivered_count(1), 1);
        assert_eq!(delivered_count(7), 7);
        assert_eq!(delivered_count(i64::MAX), u32::MAX);
    }

    #[test]
    fn test_server_max_deliver_treats_non_positive_as_unlimited() {
        assert_eq!(server_max_deliver(-1), u32::MAX);
        assert_eq!(server_max_deliver(0), u32::MAX);
        assert_eq!(server_max_deliver(7), 7);
        assert_eq!(server_max_deliver(i64::MAX), u32::MAX);
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
    async fn test_listener_reports_a_bind_failure() {
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let mut manager = NatsManager::new(event_tx);

        manager
            .listen("job", &make_trigger(), "http://127.0.0.1:4222")
            .await;

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("listener must report the failure")
            .expect("event channel closed");
        match event {
            DaemonEvent::ListenerFailed { name, error } => {
                assert_eq!(name, "job");
                assert!(!error.is_empty());
            }
            _ => panic!("expected ListenerFailed"),
        }

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
