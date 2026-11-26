//! Wake-ups for long-polling agents.
//!
//! Each namespace has a `watch` channel holding its latest event seq. Writers publish to it
//! after commit on their own instance; every instance also listens to Postgres
//! `NOTIFY concord_events`, so agents connected to other instances wake up too. A waiting
//! agent is a parked future: it holds no thread and no database connection.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub const CHANNEL: &str = "concord_events";

#[derive(Debug, Default)]
pub struct Notifier {
    senders: Mutex<HashMap<String, watch::Sender<i64>>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self, namespace: &str) -> watch::Receiver<i64> {
        let mut map = self.senders.lock().expect("notifier poisoned");
        map.entry(namespace.to_string()).or_insert_with(|| watch::channel(0).0).subscribe()
    }

    pub fn publish(&self, namespace: &str, seq: i64) {
        let mut map = self.senders.lock().expect("notifier poisoned");
        if let Some(tx) = map.get(namespace) {
            if tx.receiver_count() == 0 {
                map.remove(namespace);
                return;
            }
            tx.send_if_modified(|cur| {
                if seq > *cur {
                    *cur = seq;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// Wake every waiter (they re-check the database). Used after the listener reconnects,
    /// since notifications sent while it was down are lost.
    pub fn wake_all(&self) {
        let map = self.senders.lock().expect("notifier poisoned");
        for tx in map.values() {
            tx.send_modify(|_| {});
        }
    }
}

pub fn payload(namespace: &str, seq: i64) -> String {
    format!("{namespace}\t{seq}")
}

fn parse(payload: &str) -> Option<(&str, i64)> {
    let (ns, seq) = payload.rsplit_once('\t')?;
    Some((ns, seq.parse().ok()?))
}

/// Forwards Postgres notifications into the local notifier until cancelled.
pub async fn listen(pool: PgPool, notifier: std::sync::Arc<Notifier>, cancel: CancellationToken) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "event listener: connect failed, retrying");
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                }
            }
        };
        if let Err(e) = listener.listen(CHANNEL).await {
            tracing::warn!(error = %e, "event listener: LISTEN failed, retrying");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        notifier.wake_all();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                msg = listener.try_recv() => match msg {
                    Ok(Some(n)) => {
                        if let Some((ns, seq)) = parse(n.payload()) {
                            notifier.publish(ns, seq);
                        }
                    }
                    // Connection was lost and re-established: anything in between is gone.
                    Ok(None) => notifier.wake_all(),
                    Err(e) => {
                        tracing::warn!(error = %e, "event listener: receive failed, reconnecting");
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        assert_eq!(parse(&payload("team-a", 42)), Some(("team-a", 42)));
        assert_eq!(parse("garbage"), None);
    }

    #[tokio::test]
    async fn publish_wakes_subscribers() {
        let n = Notifier::new();
        let mut rx = n.subscribe("w");
        n.publish("w", 3);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 3);
        n.publish("w", 2); // older seq: ignored
        assert!(!rx.has_changed().unwrap());
        n.wake_all();
        assert!(rx.has_changed().unwrap());
    }
}
