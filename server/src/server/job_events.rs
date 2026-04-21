use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};
use protocol::JobEvent;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

const DEFAULT_BUFFER_PER_JOB: usize = 400;
const DEFAULT_TERMINAL_TTL_SECS: i64 = 900;
pub const DEFAULT_BROADCAST_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct JobEventHub {
    inner: Arc<RwLock<HashMap<Uuid, JobEventBuffer>>>,
    buffer_per_job: usize,
    terminal_ttl: ChronoDuration,
}

struct JobEventBuffer {
    replay: VecDeque<JobEvent>,
    tx: broadcast::Sender<JobEvent>,
    terminal_at: Option<chrono::DateTime<Utc>>,
}

impl JobEventHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            buffer_per_job: DEFAULT_BUFFER_PER_JOB,
            terminal_ttl: ChronoDuration::seconds(DEFAULT_TERMINAL_TTL_SECS),
        }
    }

    pub async fn push(&self, event: JobEvent) {
        let mut map = self.inner.write().await;
        let entry = map.entry(event.job_id).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
            JobEventBuffer {
                replay: VecDeque::new(),
                tx,
                terminal_at: None,
            }
        });

        entry.replay.push_back(event.clone());
        while entry.replay.len() > self.buffer_per_job {
            entry.replay.pop_front();
        }
        let _ = entry.tx.send(event);
    }

    pub async fn subscribe(&self, job_id: Uuid) -> (Vec<JobEvent>, broadcast::Receiver<JobEvent>) {
        let mut map = self.inner.write().await;
        let entry = map.entry(job_id).or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(DEFAULT_BROADCAST_CAPACITY);
            JobEventBuffer {
                replay: VecDeque::new(),
                tx,
                terminal_at: None,
            }
        });
        let replay = entry.replay.iter().cloned().collect::<Vec<_>>();
        let rx = entry.tx.subscribe();
        (replay, rx)
    }

    pub async fn mark_terminal(&self, job_id: Uuid) {
        let mut map = self.inner.write().await;
        if let Some(entry) = map.get_mut(&job_id) {
            entry.terminal_at = Some(Utc::now());
        }
    }

    pub async fn cleanup_terminal_expired(&self) -> usize {
        let cutoff = Utc::now() - self.terminal_ttl;
        let mut map = self.inner.write().await;
        let before = map.len();
        map.retain(|_, entry| entry.terminal_at.is_none_or(|at| at >= cutoff));
        before.saturating_sub(map.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replay_and_subscribe_receive_events() {
        let hub = JobEventHub::new();
        let job_id = Uuid::new_v4();
        let event = JobEvent {
            job_id,
            worker_id: "w1".into(),
            at: Utc::now(),
            kind: protocol::JobEventKind::System,
            stream: None,
            message: "hello".into(),
        };
        hub.push(event.clone()).await;

        let (replay, mut rx) = hub.subscribe(job_id).await;
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].message, "hello");

        let second = JobEvent {
            message: "world".into(),
            ..event
        };
        hub.push(second.clone()).await;
        let live = rx.recv().await.expect("recv live");
        assert_eq!(live.message, second.message);
    }
}
