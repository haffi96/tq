//! Robot-side changes the room should hear about as events: a camera
//! source coming or going, the dead-man tripping or re-arming. They are
//! emitted where they happen and sent to the relay on the events stream
//! (`uplink::events_loop`).

use std::sync::Arc;

use tokio::sync::broadcast;
use tq_proto::{now_us, Event};

/// Edges in flight before the uplink restates everything instead.
const BUFFER: usize = 64;

pub struct Edges {
    tx: broadcast::Sender<(Event, u64)>,
}

impl Edges {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: broadcast::channel(BUFFER).0,
        })
    }

    /// Records `event` as happening now.
    pub fn emit(&self, event: Event) {
        tracing::debug!(?event, "robot event");
        // Nobody listening (no relay connection) is fine: the uplink
        // restates the current values when it connects.
        let _ = self.tx.send((event, now_us()));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(Event, u64)> {
        self.tx.subscribe()
    }
}
