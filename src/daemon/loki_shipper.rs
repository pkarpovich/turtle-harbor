use crate::common::config::LokiConfig;
use chrono::{DateTime, Local};
use serde::Serialize;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration};

const BATCH_FLUSH_INTERVAL_MS: u64 = 2000;
const BATCH_MAX_SIZE: usize = 100;
pub const CHANNEL_CAPACITY: usize = 10_000;

pub struct LokiLogEntry {
    pub script: String,
    pub stream: String,
    pub timestamp: DateTime<Local>,
    pub content: String,
}

#[derive(Serialize)]
struct LokiPushPayload {
    streams: Vec<LokiStream>,
}

#[derive(Serialize)]
struct LokiStream {
    stream: HashMap<String, String>,
    values: Vec<[String; 2]>,
}

pub struct LokiShipper;

impl LokiShipper {
    pub fn spawn(mut rx: mpsc::Receiver<LokiLogEntry>, config: LokiConfig) -> JoinHandle<()> {
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let push_url = format!("{}/loki/api/v1/push", config.url.trim_end_matches('/'));
            let extra_labels = config.labels.unwrap_or_default();
            let mut batch: Vec<LokiLogEntry> = Vec::with_capacity(BATCH_MAX_SIZE);
            let mut flush_timer = interval(Duration::from_millis(BATCH_FLUSH_INTERVAL_MS));

            tracing::info!(url = %push_url, "Loki shipper started");

            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(entry) => {
                                batch.push(entry);
                                if batch.len() >= BATCH_MAX_SIZE {
                                    flush_batch(&client, &push_url, &extra_labels, &mut batch).await;
                                }
                            }
                            None => {
                                flush_batch(&client, &push_url, &extra_labels, &mut batch).await;
                                tracing::info!("Loki shipper shutting down");
                                break;
                            }
                        }
                    }
                    _ = flush_timer.tick() => {
                        if !batch.is_empty() {
                            flush_batch(&client, &push_url, &extra_labels, &mut batch).await;
                        }
                    }
                }
            }
        })
    }
}

type StreamKey = (String, String);

async fn flush_batch(
    client: &reqwest::Client,
    url: &str,
    extra_labels: &HashMap<String, String>,
    batch: &mut Vec<LokiLogEntry>,
) {
    if batch.is_empty() {
        return;
    }

    let mut grouped: HashMap<StreamKey, Vec<[String; 2]>> = HashMap::new();

    for entry in batch.drain(..) {
        let key = (entry.script.clone(), entry.stream.clone());
        let ts_nanos = entry
            .timestamp
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .to_string();
        grouped
            .entry(key)
            .or_default()
            .push([ts_nanos, entry.content]);
    }

    let streams: Vec<LokiStream> = grouped
        .into_iter()
        .map(|((script, stream), values)| {
            let mut labels = HashMap::new();
            labels.insert("script".to_string(), script);
            labels.insert("stream".to_string(), stream);
            for (k, v) in extra_labels {
                labels.insert(k.clone(), v.clone());
            }
            LokiStream {
                stream: labels,
                values,
            }
        })
        .collect();

    let payload = LokiPushPayload { streams };

    match client.post(url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("Loki push succeeded");
        }
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "Loki push rejected, dropping batch");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Loki push failed, dropping batch");
        }
    }
}
