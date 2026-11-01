//! One datagram track's frames on a node (owner or relay): the newest
//! frame for late joiners plus a broadcast to every live subscriber. A
//! frame is an encoded [`tq_wire::Datagram::TrackFrame`], so a relay
//! forwards the same bytes to everyone.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::broadcast;

/// Frames a subscriber may fall behind by. Older ones are simply lost, as
/// they would be on the network.
const BROADCAST_CAPACITY: usize = 64;

pub struct DatagramTrack {
    latest: Mutex<Option<Bytes>>,
    tx: broadcast::Sender<Bytes>,
}

impl DatagramTrack {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            latest: Mutex::new(None),
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
        })
    }

    pub fn push(&self, frame: Bytes) {
        let mut latest = self.latest.lock().unwrap();
        *latest = Some(frame.clone());
        // Sent under the lock so subscribe() sees latest and stream in step.
        let _ = self.tx.send(frame);
    }

    /// Forgets the newest frame (source gone).
    pub fn reset(&self) {
        *self.latest.lock().unwrap() = None;
    }

    /// The newest frame, if any, and a receiver for every frame after it.
    pub fn subscribe(&self) -> (Option<Bytes>, broadcast::Receiver<Bytes>) {
        let latest = self.latest.lock().unwrap();
        (latest.clone(), self.tx.subscribe())
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}
