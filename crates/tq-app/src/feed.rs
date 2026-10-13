//! One camera: its decoder thread, its latest picture and its numbers.
//!
//! The session pushes frames in decode order; the decoder thread decodes
//! each one as soon as it arrives and replaces the camera's picture. There
//! is no jitter buffer: the UI draws whatever picture is newest when it
//! renders.

use std::{
    sync::{mpsc, Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tq_proto::{now_us, CameraInfo, FrameHeader, FLAG_CAPTURE_FROM_SEI};

use crate::{
    session::Shared,
    stats::{Win, Window},
    vt::{self, Decoder, Picture},
};

/// A frame that waited this long for the decoder is not worth showing: skip
/// to the next keyframe rather than build latency.
const DECODE_STALL: Duration = Duration::from_millis(150);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

pub struct Job {
    pub codec: Arc<str>,
    pub header: FrameHeader,
    pub payload: Bytes,
    pub recv_us: u64,
    pub recv_at: Instant,
}

enum Msg {
    Job(Job),
    Reset,
}

#[derive(Default)]
struct Counters {
    frames: u64,
    decoded: u64,
    dropped: u64,
    keyframes: u64,
    groups: u64,
    errors: u64,
    bytes: u64,
}

#[derive(Default)]
struct Windows {
    capture_to_publisher: Win,
    publisher_to_relay: Win,
    relay_to_app: Win,
    decode: Win,
    capture_to_decoded: Win,
    capture_to_render: Win,
    decoded_to_painted: Win,
}

#[derive(Default)]
struct State {
    picture: Option<Picture>,
    /// Bumped for each new picture, so the UI can tell it has a new one.
    seq: u64,
    painted_seq: u64,
    /// Capture time of the current picture on this machine's clock.
    capture_here_us: Option<i64>,
    /// When the decoder handed back the current picture.
    decoded_at: Option<Instant>,
    codec: Option<String>,
    size: Option<(usize, usize)>,
    capture_from_sei: bool,
    last_error: Option<String>,
    c: Counters,
    w: Windows,
    last: (u64, u64, Option<Instant>),
}

/// Per-camera numbers for one report window.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub codec: Option<String>,
    pub size: Option<(usize, usize)>,
    pub fps: f64,
    pub kbps: f64,
    pub frames: u64,
    pub dropped: u64,
    pub groups: u64,
    pub errors: u64,
    pub capture_from_sei: bool,
    pub last_error: Option<String>,
    pub capture_to_publisher: Window,
    pub publisher_to_relay: Window,
    pub relay_to_app: Window,
    pub decode: Window,
    pub capture_to_decoded: Window,
    pub capture_to_render: Window,
    /// Decoder output -> the UI painted it: everything the UI adds.
    pub decoded_to_painted: Window,
}

pub struct Feed {
    pub info: CameraInfo,
    group: Mutex<Option<u64>>,
    tx: mpsc::Sender<Msg>,
    state: Mutex<State>,
}

impl Feed {
    pub fn spawn(info: CameraInfo, shared: Weak<Shared>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let feed = Arc::new(Self {
            info,
            group: Mutex::new(None),
            tx,
            state: Mutex::default(),
        });
        let weak = Arc::downgrade(&feed);
        std::thread::Builder::new()
            .name(format!("decode-{}", feed.info.id))
            .spawn(move || decode_loop(weak, shared, rx))
            .expect("spawn decoder thread");
        feed
    }

    /// Takes a new group stream if it is newer than the current one.
    pub fn start_group(&self, group: u64) -> bool {
        let mut g = self.group.lock().unwrap();
        if g.is_some_and(|cur| group <= cur) {
            return false;
        }
        *g = Some(group);
        self.state.lock().unwrap().c.groups += 1;
        true
    }

    pub fn is_current(&self, group: u64) -> bool {
        *self.group.lock().unwrap() == Some(group)
    }

    pub fn push(&self, job: Job) {
        let _ = self.tx.send(Msg::Job(job));
    }

    /// The session ended: group numbers restart and the decoder must wait
    /// for a keyframe. The last picture stays up.
    pub fn reset(&self) {
        *self.group.lock().unwrap() = None;
        let _ = self.tx.send(Msg::Reset);
    }

    /// The newest picture, for the UI.
    pub fn picture(&self) -> Option<Picture> {
        self.state.lock().unwrap().picture.clone()
    }

    /// There is a picture the UI has not painted yet.
    pub fn has_new_picture(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.seq != s.painted_seq
    }

    /// The UI painted the newest picture. The first call after a new
    /// picture records capture -> render and decoded -> painted.
    pub fn mark_painted(&self) {
        let mut s = self.state.lock().unwrap();
        if s.seq == s.painted_seq {
            return;
        }
        s.painted_seq = s.seq;
        if let Some(cap) = s.capture_here_us {
            s.w.capture_to_render.add((now_us() as i64 - cap) as f64 / 1000.0);
        }
        if let Some(t) = s.decoded_at {
            s.w.decoded_to_painted.add(t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    pub fn take_stats(&self) -> Stats {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        let secs = s.last.2.map_or(0.5, |t| now.duration_since(t).as_secs_f64()).max(1e-3);
        let fps = (s.c.decoded - s.last.0) as f64 / secs;
        let kbps = (s.c.bytes - s.last.1) as f64 * 8.0 / 1000.0 / secs;
        s.last = (s.c.decoded, s.c.bytes, Some(now));
        Stats {
            codec: s.codec.clone(),
            size: s.size,
            fps,
            kbps,
            frames: s.c.frames,
            dropped: s.c.dropped,
            groups: s.c.groups,
            errors: s.c.errors,
            capture_from_sei: s.capture_from_sei,
            last_error: s.last_error.clone(),
            capture_to_publisher: s.w.capture_to_publisher.take(),
            publisher_to_relay: s.w.publisher_to_relay.take(),
            relay_to_app: s.w.relay_to_app.take(),
            decode: s.w.decode.take(),
            capture_to_decoded: s.w.capture_to_decoded.take(),
            capture_to_render: s.w.capture_to_render.take(),
            decoded_to_painted: s.w.decoded_to_painted.take(),
        }
    }
}

fn decode_loop(feed: Weak<Feed>, shared: Weak<Shared>, rx: mpsc::Receiver<Msg>) {
    let mut decoder: Option<Decoder> = None;
    let mut wait_key = true;
    let mut last_key_request: Option<Instant> = None;
    // When the last keyframe finished decoding. Frames that queued behind
    // it (the join burst of the relay's cached GOP, or a new decoder
    // session opening) are not a backlog.
    let mut key_done = Instant::now();

    while let Ok(msg) = rx.recv() {
        let (Some(feed), Some(shared)) = (feed.upgrade(), shared.upgrade()) else {
            return;
        };
        let job = match msg {
            Msg::Job(job) => job,
            Msg::Reset => {
                decoder = None;
                wait_key = true;
                continue;
            }
        };
        let mut request_keyframe = |reason: &str| {
            if last_key_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL) {
                last_key_request = Some(Instant::now());
                shared.request_keyframe(&feed.info.id, reason);
            }
        };
        let h = job.header;
        let key = h.is_key();
        let clock = shared.clock.lock().unwrap().clone();
        let capture_here = match (clock.pub_offset_us, clock.my_offset_us) {
            // Capture time on this clock: publisher clock -> relay -> here.
            (Some(p), Some(m)) => Some(h.capture_us as i64 - p + m),
            _ => None,
        };
        {
            let mut s = feed.state.lock().unwrap();
            s.c.frames += 1;
            s.c.bytes += job.payload.len() as u64;
            s.capture_from_sei = h.flags & FLAG_CAPTURE_FROM_SEI != 0;
            s.w.capture_to_publisher.add((h.publisher_us as f64 - h.capture_us as f64) / 1000.0);
            if let (Some(p), Some(m)) = (clock.pub_offset_us, clock.my_offset_us) {
                s.w.publisher_to_relay.add((h.relay_us as i64 - (h.publisher_us as i64 - p)) as f64 / 1000.0);
                s.w.relay_to_app.add((job.recv_us as i64 - m - h.relay_us as i64) as f64 / 1000.0);
            }
            if key {
                s.c.keyframes += 1;
            }
        }

        if key {
            match vt::codec_of(&job.codec) {
                Ok(codec) if decoder.as_ref().is_some_and(|d| d.codec() == codec) => {}
                Ok(codec) => decoder = Some(Decoder::new(codec)),
                Err(e) => {
                    feed.state.lock().unwrap().last_error = Some(e.to_string());
                    decoder = None;
                }
            }
        }
        let Some(dec) = decoder.as_mut().filter(|_| key || !wait_key) else {
            feed.state.lock().unwrap().c.dropped += 1;
            continue;
        };
        if !key && job.recv_at > key_done && job.recv_at.elapsed() > DECODE_STALL {
            feed.state.lock().unwrap().c.dropped += 1;
            wait_key = true;
            request_keyframe("decoder backlog");
            continue;
        }

        let t0 = Instant::now();
        let result = dec.decode(&job.payload, key);
        let decode_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if key {
            key_done = Instant::now();
        }
        let mut s = feed.state.lock().unwrap();
        match result {
            Ok(Some(picture)) => {
                wait_key = false;
                s.c.decoded += 1;
                s.w.decode.add(decode_ms);
                if let Some(cap) = capture_here {
                    s.w.capture_to_decoded.add((now_us() as i64 - cap) as f64 / 1000.0);
                }
                s.size = Some(picture.size());
                s.codec = Some(job.codec.to_string());
                s.picture = Some(picture);
                s.capture_here_us = capture_here;
                s.decoded_at = Some(Instant::now());
                s.seq += 1;
                drop(s);
                shared.frame_ready();
            }
            Ok(None) => {
                wait_key = false;
                s.c.dropped += 1;
            }
            Err(e) => {
                s.c.errors += 1;
                s.last_error = Some(format!("{e:#}"));
                drop(s);
                tracing::warn!(camera = %feed.info.id, "decode: {e:#}");
                wait_key = true;
                request_keyframe("decoder error");
            }
        }
    }
}
