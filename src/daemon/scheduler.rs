use crate::common::error::{Error, Result};
use crate::daemon::daemon_core::DaemonEvent;
use chrono::Local;
use cron::Schedule;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub fn spawn_cron_task(
    name: String,
    cron_expr: &str,
    event_tx: mpsc::Sender<DaemonEvent>,
) -> Result<JoinHandle<()>> {
    let schedule = Schedule::from_str(cron_expr).map_err(|source| Error::CronParse {
        expression: cron_expr.to_string(),
        source,
    })?;

    let handle = tokio::spawn(async move {
        loop {
            let next = match schedule.upcoming(Local).next() {
                Some(t) => t,
                None => return,
            };

            let now = Local::now();
            if next > now {
                let duration = next
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or_default();
                tokio::time::sleep(duration).await;
            }

            if event_tx
                .send(DaemonEvent::CronTick {
                    name: name.clone(),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    Ok(handle)
}
