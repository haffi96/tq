//! One camera's frames on a node (publisher or relay): the current GOP for
//! late joiners plus a broadcast to every live subscriber.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;
use tq_proto::MediaFrame;

/// Frames kept for late joiners. A GOP longer than this is not cached and a
/// new subscriber waits for the next keyframe.
const MAX_CACHED_FRAMES: usize = 600;
const BROADCAST_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    Accepted,
    /// Not the next frame of the current group (older group, or a gap).
    Dropped,
}

struct Inner {
    /// Group number on the incoming side, used to match frames to groups.
    in_group: Option<u64>,
    /// Group number this track assigns on the outgoing side. Monotonic for
    /// the lifetime of the track, whatever the source does.
    out_group: u64,
    next_index: u32,
    /// The current group from its keyframe, when it is short enough.
    cache: Vec<Arc<MediaFrame>>,
    cache_valid: bool,
}

pub struct Track {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Arc<MediaFrame>>,
}

impl Track {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                in_group: None,
                out_group: 0,
                next_index: 0,
                cache: Vec::new(),
                cache_valid: false,
            }),
            tx,
        })
    }

    /// Adds a frame from the source. A keyframe at index 0 starts a new
    /// group; anything else must be the next frame of the current group.
    /// The frame is re-stamped with this track's group number and camera
    /// index before it is cached and broadcast.
    pub fn push(&self, mut frame: MediaFrame, camera: u16) -> Push {
        let mut inner = self.inner.lock().unwrap();
        if frame.is_key() && frame.header.index == 0 {
            inner.in_group = Some(frame.group);
            inner.out_group += 1;
            inner.next_index = 0;
            inner.cache.clear();
            inner.cache_valid = true;
        } else if inner.in_group != Some(frame.group) {
            // An older group still trickling in after a newer one started.
            return Push::Dropped;
        } else if frame.header.index != inner.next_index {
            // A gap in the current group: everything after it is
            // undecodable, so wait for the next keyframe.
            inner.in_group = None;
            return Push::Dropped;
        }
        inner.next_index = frame.header.index + 1;
        frame.group = inner.out_group;
        frame.camera = camera;
        let frame = Arc::new(frame);
        if inner.cache_valid {
            if inner.cache.len() < MAX_CACHED_FRAMES {
                inner.cache.push(frame.clone());
            } else {
                inner.cache.clear();
                inner.cache_valid = false;
            }
        }
        // Sent under the lock so subscribe() sees cache and stream in step.
        let _ = self.tx.send(frame);
        Push::Accepted
    }

    /// Forgets the current group (source disconnected). Subscribers keep
    /// their receivers and resume at the next keyframe.
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.in_group = None;
        inner.cache.clear();
        inner.cache_valid = false;
    }

    /// The current group so far (empty when unknown) and a receiver for
    /// every frame after it.
    pub fn subscribe(&self) -> (Vec<Arc<MediaFrame>>, broadcast::Receiver<Arc<MediaFrame>>) {
        let inner = self.inner.lock().unwrap();
        let cache = if inner.cache_valid && inner.in_group.is_some() {
            inner.cache.clone()
        } else {
            Vec::new()
        };
        (cache, self.tx.subscribe())
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tq_proto::{FrameHeader, FLAG_KEY};

    fn frame(group: u64, index: u32) -> MediaFrame {
        MediaFrame {
            camera: 9,
            group,
            codec: "avc1.42c01f".into(),
            header: FrameHeader {
                index,
                flags: if index == 0 { FLAG_KEY } else { 0 },
                ..Default::default()
            },
            payload: Bytes::new(),
        }
    }

    #[test]
    fn caches_current_group_and_renumbers() {
        let t = Track::new();
        assert_eq!(t.push(frame(7, 1), 0), Push::Dropped);
        assert_eq!(t.push(frame(7, 0), 0), Push::Accepted);
        assert_eq!(t.push(frame(7, 1), 0), Push::Accepted);
        let (cache, mut rx) = t.subscribe();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[0].group, 1);
        assert_eq!(cache[0].camera, 0);
        // Source restarts with a lower group number: still a new group.
        assert_eq!(t.push(frame(0, 0), 0), Push::Accepted);
        let f = rx.try_recv().unwrap();
        assert_eq!((f.group, f.header.index), (2, 0));
        // A frame of the superseded group is dropped without harming the
        // current one.
        assert_eq!(t.push(frame(7, 2), 0), Push::Dropped);
        assert_eq!(t.subscribe().0.len(), 1);
        // A gap drops the rest of the group.
        assert_eq!(t.push(frame(0, 2), 0), Push::Dropped);
        assert_eq!(t.push(frame(0, 1), 0), Push::Dropped);
        assert!(t.subscribe().0.is_empty());
    }
}
