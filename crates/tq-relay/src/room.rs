//! Rooms: one per robot, holding its publisher, operators and viewers.
//!
//! Every change participants should hear about goes through a method here
//! that changes the state and emits its [`Event`] under the same lock, so
//! the event sequence and the state never disagree. Events are encoded
//! once and fanned out to every session's events stream through a
//! broadcast channel; a session that falls behind gets a fresh state
//! instead (see `client::events`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::{broadcast, watch};
use tq_proto::{
    now_us, BuildInfo, CameraInfo, CameraStatus, Control, Event, OperatorChange, ParticipantInfo, PublisherGone,
    PublisherStatus, QuicStats, RoomEvent, RoomMessage, RoomState, TeleopStatus, TrackInfo,
};
use tq_transport::{control::ControlTx, track::Track, Session};

/// Events a session may fall behind by before it is resynced with a state.
const EVENT_BUFFER: usize = 256;
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

pub struct PublisherLink {
    pub conn_id: u64,
    pub publisher_id: String,
    pub build: Option<BuildInfo>,
    pub session: Session,
    pub ctl: ControlTx,
    pub cameras: Vec<CameraInfo>,
    pub camera_status: Vec<CameraStatus>,
    pub teleop: TeleopStatus,
    pub rtt_ms: Option<f64>,
    pub clock: tq_proto::ClockFilter,
    pub quic: Option<QuicStats>,
}

pub struct ClientLink {
    pub info: ParticipantInfo,
    pub session: Session,
}

#[derive(Default)]
pub struct RoomInner {
    /// Sequence number of the last event.
    seq: u64,
    pub publisher: Option<PublisherLink>,
    /// Bumped on every announce; a grace timer only unpublishes the
    /// tracks of the generation it was started for.
    publisher_gen: u64,
    /// Tracks by camera id; they outlive publisher reconnects so viewers
    /// resume without resubscribing.
    pub tracks: HashMap<String, Arc<Track>>,
    /// Camera list from the last announce (index = group stream camera).
    pub cameras: Vec<CameraInfo>,
    /// Whether each camera's source is producing, from the publisher's
    /// events. Offline while there is no publisher.
    online: HashMap<String, bool>,
    /// `(tripped, reason)` from the publisher's events; `None` while there
    /// is no publisher or it has not said yet.
    deadman: Option<(bool, String)>,
    pub operator: Option<String>,
    pub clients: HashMap<String, ClientLink>,
    keyframe_requests: HashMap<String, Instant>,
}

/// An event, numbered and encoded for the events stream.
#[derive(Clone)]
pub struct Sequenced {
    pub seq: u64,
    pub bytes: Bytes,
}

pub struct Room {
    pub id: String,
    pub inner: Mutex<RoomInner>,
    /// Bumped on every change a participant should see (events and live
    /// numbers alike).
    pub version: watch::Sender<u64>,
    /// Bumped when the camera list changes (video senders restart).
    pub cameras_version: watch::Sender<u64>,
    events: broadcast::Sender<Sequenced>,
    /// The encoded state for a `version`, shared by every session.
    snapshot: Mutex<Option<(u64, Sequenced)>>,
    /// How long tracks stay published after the publisher drops.
    track_grace: Duration,
}

impl Room {
    pub fn new(id: String, track_grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            id,
            inner: Mutex::new(RoomInner::default()),
            version: watch::channel(0).0,
            cameras_version: watch::channel(0).0,
            events: broadcast::channel(EVENT_BUFFER).0,
            snapshot: Mutex::new(None),
            track_grace,
        })
    }

    pub fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    /// Numbers, encodes and broadcasts an event. Call with the state change
    /// it describes already made, under the same lock.
    fn emit(&self, inner: &mut RoomInner, event: Event, ts_us: u64) {
        inner.seq += 1;
        let seq = inner.seq;
        tracing::debug!(room = %self.id, seq, ?event, "event");
        let bytes = RoomMessage::Event(RoomEvent { seq, ts_us, event }).encode();
        // No receivers is fine: nobody is watching.
        let _ = self.events.send(Sequenced { seq, bytes });
        self.bump();
    }

    /// Events from now on. Subscribe before taking the [`Self::snapshot`]
    /// that they continue.
    pub fn subscribe(&self) -> broadcast::Receiver<Sequenced> {
        self.events.subscribe()
    }

    /// The current state as an encoded `state` message, built at most once
    /// per change however many sessions ask.
    pub fn snapshot(&self) -> Sequenced {
        let version = *self.version.borrow();
        let mut cache = self.snapshot.lock().unwrap();
        if let Some((v, s)) = cache.as_ref() {
            if *v == version {
                return s.clone();
            }
        }
        let (seq, state) = {
            let inner = self.inner.lock().unwrap();
            (inner.seq, self.state_of(&inner))
        };
        let s = Sequenced {
            seq,
            bytes: RoomMessage::State { seq, state }.encode(),
        };
        *cache = Some((version, s.clone()));
        s
    }

    pub fn state(&self) -> RoomState {
        self.state_of(&self.inner.lock().unwrap())
    }

    fn state_of(&self, inner: &RoomInner) -> RoomState {
        let online = |id: &str| inner.online.get(id).copied().unwrap_or(false);
        let mut participants: Vec<_> = inner.clients.values().map(|c| c.info.clone()).collect();
        participants.sort_by(|a, b| (a.joined_us, &a.session).cmp(&(b.joined_us, &b.session)));
        RoomState {
            room_id: self.id.clone(),
            publisher: inner.publisher.as_ref().map(|p| {
                // Online and dead-man come from events, so that the state
                // matches the event sequence; the rest is live numbers.
                let mut camera_status = p.camera_status.clone();
                for c in &mut camera_status {
                    c.online = online(&c.id);
                }
                let mut teleop = p.teleop.clone();
                if let Some((tripped, reason)) = &inner.deadman {
                    teleop.deadman_tripped = *tripped;
                    teleop.reason = reason.clone();
                }
                PublisherStatus {
                    publisher_id: p.publisher_id.clone(),
                    build: p.build.clone(),
                    cameras: p.cameras.clone(),
                    camera_status,
                    teleop,
                    rtt_ms: p.rtt_ms,
                    clock_offset_us: p.clock.estimate().map(|(o, _)| o),
                    clock_error_us: p.clock.estimate().map(|(_, e)| e),
                    quic: p.quic.clone(),
                }
            }),
            tracks: inner
                .cameras
                .iter()
                .map(|c| TrackInfo {
                    camera: c.clone(),
                    online: online(&c.id),
                })
                .collect(),
            participants,
            operator: inner.operator.clone(),
        }
    }

    /// Cameras and their tracks, in announce order.
    pub fn tracks(&self) -> Vec<(u16, CameraInfo, Arc<Track>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .cameras
            .iter()
            .enumerate()
            .filter_map(|(i, c)| inner.tracks.get(&c.id).map(|t| (i as u16, c.clone(), t.clone())))
            .collect()
    }

    // --- participants ---------------------------------------------------

    pub fn join(&self, link: ClientLink) {
        let mut inner = self.inner.lock().unwrap();
        let participant = link.info.clone();
        inner.clients.insert(participant.session.clone(), link);
        self.emit(&mut inner, Event::ParticipantJoined { participant }, now_us());
    }

    pub fn leave(&self, session: &str) {
        let was_operator = {
            let mut inner = self.inner.lock().unwrap();
            let Some(link) = inner.clients.remove(session) else {
                return;
            };
            let was_operator = inner.operator.as_deref() == Some(session);
            if was_operator {
                inner.operator = None;
                let event = Event::OperatorChanged {
                    from: Some(session.to_string()),
                    to: None,
                    reason: OperatorChange::Left,
                };
                self.emit(&mut inner, event, now_us());
            }
            self.emit(&mut inner, Event::ParticipantLeft { participant: link.info }, now_us());
            was_operator
        };
        if was_operator {
            tracing::info!(room = %self.id, %session, "operator left, claim released");
            self.notify_operator();
        }
    }

    /// Takes the operator claim: `Ok(holder)` or `Err(holder)` when someone
    /// else holds it and `force` is not set.
    pub fn claim(&self, session: &str, force: bool) -> Result<String, String> {
        let previous = {
            let mut inner = self.inner.lock().unwrap();
            let previous = match inner.operator.clone() {
                Some(h) if h == session => return Ok(h),
                Some(h) if !force => return Err(h),
                prev => prev,
            };
            inner.operator = Some(session.to_string());
            let event = Event::OperatorChanged {
                reason: if previous.is_some() {
                    OperatorChange::Forced
                } else {
                    OperatorChange::Claimed
                },
                from: previous.clone(),
                to: Some(session.to_string()),
            };
            self.emit(&mut inner, event, now_us());
            previous
        };
        tracing::info!(room = %self.id, %session, taken_from = ?previous, "operator claim");
        self.notify_operator();
        Ok(session.to_string())
    }

    /// Gives up the claim if `session` holds it.
    pub fn release(&self, session: &str) -> bool {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.operator.as_deref() != Some(session) {
                return false;
            }
            inner.operator = None;
            let event = Event::OperatorChanged {
                from: Some(session.to_string()),
                to: None,
                reason: OperatorChange::Released,
            };
            self.emit(&mut inner, event, now_us());
        }
        tracing::info!(room = %self.id, %session, "operator released claim");
        self.notify_operator();
        true
    }

    // --- publisher ------------------------------------------------------

    /// Makes `link` the room's publisher; returns the one it replaces.
    /// Compares the announced cameras with the room's tracks and emits
    /// what changed.
    pub fn publisher_connected(&self, link: PublisherLink) -> Option<PublisherLink> {
        let now = now_us();
        let mut inner = self.inner.lock().unwrap();
        inner.publisher_gen += 1;
        let (publisher_id, build) = (link.publisher_id.clone(), link.build.clone());
        let cameras = link.cameras.clone();
        let old = inner.publisher.replace(link);
        if let Some(old) = &old {
            let event = Event::PublisherDisconnected {
                publisher_id: old.publisher_id.clone(),
                reason: PublisherGone::Replaced,
            };
            self.emit(&mut inner, event, now);
        }
        self.emit(&mut inner, Event::PublisherConnected { publisher_id, build }, now);

        let previous = std::mem::take(&mut inner.cameras);
        for gone in previous.iter().filter(|p| !cameras.iter().any(|c| c.id == p.id)) {
            inner.tracks.remove(&gone.id);
            inner.online.remove(&gone.id);
            let event = Event::TrackUnpublished {
                camera: gone.id.clone(),
                reason: "not in the publisher's announce".into(),
            };
            self.emit(&mut inner, event, now);
        }
        for c in &cameras {
            inner.tracks.entry(c.id.clone()).or_insert_with(Track::new);
            let online = inner.online.get(&c.id).copied().unwrap_or(false);
            let track = TrackInfo {
                camera: c.clone(),
                online,
            };
            match previous.iter().find(|p| p.id == c.id) {
                None => self.emit(&mut inner, Event::TrackPublished { track }, now),
                Some(p) if p != c => self.emit(&mut inner, Event::TrackUpdated { track }, now),
                Some(_) => {}
            }
        }
        inner.cameras = cameras;
        if inner.cameras != previous {
            self.cameras_version.send_modify(|v| *v += 1);
        }
        old
    }

    /// The publisher session `conn_id` ended. Its tracks go offline now and
    /// are unpublished if no publisher returns within the grace period.
    pub fn publisher_disconnected(self: &Arc<Self>, conn_id: u64) {
        let now = now_us();
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            if inner.publisher.as_ref().is_none_or(|p| p.conn_id != conn_id) {
                return;
            }
            let p = inner.publisher.take().expect("checked above");
            for t in inner.tracks.values() {
                t.reset();
            }
            inner.deadman = None;
            for c in inner.cameras.clone() {
                if inner.online.insert(c.id.clone(), false) == Some(true) {
                    let event = Event::TrackOffline {
                        camera: c.id,
                        reason: "publisher disconnected".into(),
                    };
                    self.emit(&mut inner, event, now);
                }
            }
            let event = Event::PublisherDisconnected {
                publisher_id: p.publisher_id,
                reason: PublisherGone::Disconnected,
            };
            self.emit(&mut inner, event, now);
            inner.publisher_gen
        };
        let room = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(room.track_grace).await;
            room.unpublish_abandoned(generation);
        });
    }

    fn unpublish_abandoned(&self, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.publisher.is_some() || inner.publisher_gen != generation || inner.cameras.is_empty() {
            return;
        }
        tracing::info!(room = %self.id, "publisher did not return, unpublishing its tracks");
        let now = now_us();
        for c in std::mem::take(&mut inner.cameras) {
            inner.tracks.remove(&c.id);
            inner.online.remove(&c.id);
            let event = Event::TrackUnpublished {
                camera: c.id,
                reason: "publisher did not return".into(),
            };
            self.emit(&mut inner, event, now);
        }
        self.cameras_version.send_modify(|v| *v += 1);
    }

    /// A robot-side event from publisher session `conn_id`, stamped
    /// `ts_us` on the publisher's clock. Applied and re-emitted only if it
    /// changes something.
    pub fn robot_event(&self, conn_id: u64, event: Event, ts_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(p) = inner.publisher.as_ref().filter(|p| p.conn_id == conn_id) else {
            return;
        };
        // On the relay's clock; never in the future.
        let offset = p.clock.estimate().map_or(0, |(o, _)| o);
        let ts_us = (ts_us as i64 - offset).clamp(0, now_us() as i64) as u64;
        let changed = match &event {
            Event::TrackOnline { camera } | Event::TrackOffline { camera, .. } => {
                let online = matches!(event, Event::TrackOnline { .. });
                inner.cameras.iter().any(|c| &c.id == camera)
                    && inner.online.insert(camera.clone(), online).unwrap_or(false) != online
            }
            Event::DeadmanTripped { reason } => {
                let now = (true, reason.clone());
                inner.deadman.replace(now.clone()).is_none_or(|was| was.0 != now.0)
            }
            Event::DeadmanCleared => inner.deadman.replace((false, String::new())).is_none_or(|was| was.0),
            other => {
                tracing::debug!(room = %self.id, ?other, "ignored publisher event");
                false
            }
        };
        if changed {
            self.emit(&mut inner, event, ts_us);
        } else {
            // A reason change while tripped is shown, but is no edge.
            self.bump();
        }
    }

    /// Tells the publisher which session holds the operator claim.
    pub fn notify_operator(&self) {
        let (ctl, session) = {
            let inner = self.inner.lock().unwrap();
            (inner.publisher.as_ref().map(|p| p.ctl.clone()), inner.operator.clone())
        };
        if let Some(ctl) = ctl {
            tokio::spawn(async move {
                let _ = ctl.send(&Control::Operator { session }).await;
            });
        }
    }

    /// Forwards a keyframe request to the publisher, at most one per camera
    /// per 500 ms.
    pub fn request_keyframe(&self, camera: &str, reason: &str) {
        let ctl = {
            let mut inner = self.inner.lock().unwrap();
            let now = Instant::now();
            if inner
                .keyframe_requests
                .get(camera)
                .is_some_and(|t| now - *t < KEYFRAME_REQUEST_INTERVAL)
            {
                return;
            }
            inner.keyframe_requests.insert(camera.to_string(), now);
            inner.publisher.as_ref().map(|p| p.ctl.clone())
        };
        if let Some(ctl) = ctl {
            let msg = Control::KeyframeRequest {
                camera: camera.to_string(),
                reason: reason.to_string(),
            };
            tokio::spawn(async move {
                let _ = ctl.send(&msg).await;
            });
        }
    }
}
