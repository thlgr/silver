//! Per-run event emitter. The core assigns the monotonic sequence number so persistence
//! and SSE order match without a shared coordinator.

use chrono::Utc;
use silver_protocol::{EventId, EventPayload, RunEvent, RunId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct EventEmitter {
    inner: Arc<Inner>,
}

struct Inner {
    run_id: RunId,
    seq: AtomicU64,
    tx: mpsc::UnboundedSender<RunEvent>,
}

impl EventEmitter {
    /// Create an emitter and the receiver the daemon persists/streams from.
    pub fn channel(run_id: RunId) -> (Self, mpsc::UnboundedReceiver<RunEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::from_sender(run_id, tx), rx)
    }

    pub fn from_sender(run_id: RunId, tx: mpsc::UnboundedSender<RunEvent>) -> Self {
        Self {
            inner: Arc::new(Inner {
                run_id,
                seq: AtomicU64::new(0),
                tx,
            }),
        }
    }

    pub fn run_id(&self) -> RunId {
        self.inner.run_id
    }

    /// Assign the next sequence number, timestamp, and publish the event.
    pub fn emit(&self, payload: EventPayload) {
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = RunEvent {
            run_id: self.inner.run_id,
            event_id: EventId(seq),
            created_at: Utc::now(),
            payload,
        };
        drop(self.inner.tx.send(event));
    }

    /// Last assigned sequence number (0 before the first event).
    pub fn last_seq(&self) -> u64 {
        self.inner.seq.load(Ordering::SeqCst)
    }
}
