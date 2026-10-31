//! Rooms: the sessions in one namespace, the broadcasts they publish, who
//! subscribes to what and who holds which lock.
//!
//! Every change sessions should hear about goes through a method here that
//! changes the state and emits its [`Event`] under the same lock, so the
//! event sequence and the state never disagree. Events are encoded once
//! and fanned out to every session's events stream through a broadcast
//! channel; a session that falls behind gets a fresh state instead (see
//! `session::pump_events`).
//!
//! Messages to a session's control stream go through its [`Outbox`], a
//! queue drained by one task, so they arrive in the order they were
//! decided here (a lock holder change and its reversal never swap).

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, watch};
use tq_transport::{dgtrack::DatagramTrack, track::Track, Session};
use tq_wire::{
    now_us, Advertisement, BroadcastState, BuildInfo, ChannelState, ClockFilter, Control, Event, LockChange,
    LockState, ParticipantInfo, Path, Perms, QuicStats, RoomEvent, RoomMessage, RoomState, StopReason, TrackInfo,
    TrackState, TrackStats, Verb,
};

/// Events a session may fall behind by before it is resynced with a state.
const EVENT_BUFFER: usize = 256;
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Control messages to one session, in order.
pub type Outbox = mpsc::UnboundedSender<Control>;

pub struct SessionLink {
    pub info: ParticipantInfo,
    pub session: Session,
    pub out: Outbox,
    pub clock: ClockFilter,
    pub rtt_ms: Option<f64>,
    pub quic: Option<QuicStats>,
    /// Broadcasts this session owns, by its own (owner hop) index. Indices
    /// are not reused within a connection.
    pub owned: Vec<Option<String>>,
}

impl SessionLink {
    pub fn perms(&self) -> &Perms {
        &self.info.perms
    }
}

/// What the relay counts as a track's frames arrive.
#[derive(Default)]
pub struct Counters {
    pub frames: AtomicU64,
    pub groups: AtomicU64,
    pub bytes: AtomicU64,
    pub capture_lat_sum_us: AtomicU64,
    pub capture_lat_n: AtomicU64,
    pub codec_string: Mutex<Option<String>>,
}

/// A track on the relay. It outlives owner reconnects, so subscribers
/// resume without resubscribing. Only one of the two caches is used,
/// depending on the track's delivery.
#[derive(Clone)]
pub struct RelayTrack {
    pub groups: Arc<Track>,
    pub datagrams: Arc<DatagramTrack>,
    pub counters: Arc<Counters>,
}

impl RelayTrack {
    fn new() -> Self {
        Self {
            groups: Track::new(),
            datagrams: DatagramTrack::new(),
            counters: Default::default(),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Rates {
    fps: f64,
    kbps: f64,
    capture_ms: Option<f64>,
    /// Counter values at the last tick.
    frames: u64,
    bytes: u64,
}

pub struct Broadcast {
    /// The room's index, for the subscriber hop.
    pub index: u16,
    pub name: String,
    pub owner: Option<u32>,
    /// The owner's index for it, for the owner hop.
    pub owner_index: u16,
    /// Bumped whenever the owner changes or goes; a grace timer only
    /// removes the broadcast for the generation it was started for.
    generation: u64,
    pub ad: Advertisement,
    pub build: Option<BuildInfo>,
    pub tracks: HashMap<String, RelayTrack>,
    online: HashMap<String, bool>,
    /// Lock id -> holder sid.
    holders: HashMap<String, u32>,
    /// Owner-reported drops per track.
    dropped: HashMap<String, u64>,
    rates: HashMap<String, Rates>,
}

impl Broadcast {
    pub fn path(&self, name: &str) -> Path {
        Path::new(&self.name, name)
    }
}

#[derive(Default)]
pub struct RoomInner {
    /// Sequence number of the last event.
    seq: u64,
    next_sid: u32,
    next_broadcast: u16,
    pub sessions: HashMap<u32, SessionLink>,
    pub broadcasts: BTreeMap<String, Broadcast>,
    /// Who subscribes to each track (existing or not yet).
    demand: HashMap<Path, HashSet<u32>>,
    keyframe_requests: HashMap<Path, Instant>,
}

impl RoomInner {
    fn broadcast_at(&self, index: u16) -> Option<&Broadcast> {
        self.broadcasts.values().find(|b| b.index == index)
    }

    /// The broadcast a session owns under its own index.
    fn owned(&self, sid: u32, owner_index: u16) -> Option<&Broadcast> {
        let name = self.sessions.get(&sid)?.owned.get(owner_index as usize)?.as_ref()?;
        self.broadcasts.get(name).filter(|b| b.owner == Some(sid))
    }

    fn session_name(&self, sid: u32) -> Option<String> {
        self.sessions.get(&sid).map(|s| s.info.session.clone())
    }

    fn send(&self, sid: u32, msg: Control) {
        if let Some(s) = self.sessions.get(&sid) {
            let _ = s.out.send(msg);
        }
    }

    fn alloc_broadcast_index(&mut self) -> u16 {
        loop {
            let i = self.next_broadcast;
            self.next_broadcast = self.next_broadcast.wrapping_add(1);
            if self.broadcast_at(i).is_none() {
                return i;
            }
        }
    }

    /// Tells a broadcast's owner who holds `lock`.
    fn notify_holder(&self, b: &Broadcast, lock: &str) {
        if let Some(owner) = b.owner {
            let sid = b.holders.get(lock).copied();
            self.send(
                owner,
                Control::LockHolder {
                    lock: b.path(lock),
                    sid,
                    session: sid.and_then(|s| self.session_name(s)),
                },
            );
        }
    }

    /// Tells a track's owner how many subscribe to it.
    fn notify_demand(&self, path: &Path) {
        let Some(b) = self.broadcasts.get(&path.broadcast) else {
            return;
        };
        if let (Some(owner), Some(_)) = (b.owner, b.ad.track(&path.name)) {
            let subscribers = self.demand.get(path).map_or(0, |s| s.len()) as u32;
            self.send(
                owner,
                Control::TrackDemand {
                    track: path.clone(),
                    subscribers,
                },
            );
        }
    }
}

/// An event, numbered and encoded for the events stream.
#[derive(Clone)]
pub struct Sequenced {
    pub seq: u64,
    pub bytes: Bytes,
}

/// What a subscription sender needs to send a track's frames.
#[derive(Clone)]
pub struct Resolved {
    pub broadcast: u16,
    pub track: u16,
    pub info: TrackInfo,
    pub relay: RelayTrack,
}

pub struct Room {
    pub id: String,
    pub inner: Mutex<RoomInner>,
    /// Bumped on every change a session should see (events and live
    /// numbers alike).
    pub version: watch::Sender<u64>,
    /// Bumped when broadcasts, tracks or their indices change, so
    /// subscription senders re-resolve.
    pub layout: watch::Sender<u64>,
    events: broadcast::Sender<Sequenced>,
    /// The encoded state for a `version`, shared by every session.
    snapshot: Mutex<Option<(u64, Sequenced)>>,
    /// How long a broadcast stays after its owner drops.
    grace: Duration,
}

/// What [`Room::advertise`] did.
pub struct Advertised {
    pub owner_index: u16,
    /// A session that owned the broadcast until now.
    pub replaced: Option<u32>,
}

impl Room {
    pub fn new(id: String, grace: Duration) -> Arc<Self> {
        let room = Arc::new(Self {
            id,
            inner: Mutex::new(RoomInner {
                next_sid: 1,
                ..Default::default()
            }),
            version: watch::channel(0).0,
            layout: watch::channel(0).0,
            events: broadcast::channel(EVENT_BUFFER).0,
            snapshot: Mutex::new(None),
            grace,
        });
        tokio::spawn(stats_ticker(Arc::downgrade(&room)));
        room
    }

    pub fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn bump_layout(&self) {
        self.layout.send_modify(|v| *v += 1);
    }

    /// Numbers, encodes and broadcasts an event. Call with the state change
    /// it describes already made, under the same lock.
    fn emit(&self, inner: &mut RoomInner, event: Event, ts_us: u64) {
        inner.seq += 1;
        let seq = inner.seq;
        tracing::debug!(room = %self.id, seq, ?event, "event");
        let bytes = RoomMessage::Event(RoomEvent { seq, ts_us, event }).encode();
        // Bump first: a session that subscribes after the send below must
        // not get a cached snapshot from before this event (it would
        // never see the event). Its snapshot waits for this lock instead,
        // and so includes it.
        self.bump();
        // No receivers is fine: nobody is watching.
        let _ = self.events.send(Sequenced { seq, bytes });
    }

    /// Emits an arbitrary event, for tests of the events stream.
    #[cfg(test)]
    pub fn emit_for_test(&self, event: Event) {
        let mut inner = self.inner.lock().unwrap();
        self.emit(&mut inner, event, now_us());
    }

    /// Events from now on. Subscribe before taking the [`Self::snapshot`]
    /// that they continue.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Sequenced> {
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
        let mut participants: Vec<_> = inner.sessions.values().map(|s| s.info.clone()).collect();
        participants.sort_by_key(|p| (p.joined_us, p.sid));
        let broadcasts = inner
            .broadcasts
            .values()
            .map(|b| {
                let owner = b.owner.and_then(|sid| inner.sessions.get(&sid));
                let clock = owner.and_then(|o| o.clock.estimate());
                BroadcastState {
                    index: b.index,
                    name: b.name.clone(),
                    owner: owner.map(|o| o.info.session.clone()),
                    owner_sid: b.owner,
                    build: b.build.clone(),
                    rtt_ms: owner.and_then(|o| o.rtt_ms),
                    clock_offset_us: clock.map(|c| c.offset_us),
                    clock_error_us: clock.map(|c| c.error_us),
                    quic: owner.and_then(|o| o.quic.clone()),
                    tracks: b
                        .ad
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let r = b.rates.get(&t.id).copied().unwrap_or_default();
                            let c = b.tracks.get(&t.id).map(|r| r.counters.clone());
                            TrackState {
                                index: i as u16,
                                info: t.clone(),
                                online: b.online.get(&t.id).copied().unwrap_or(false),
                                stats: TrackStats {
                                    frames: c.as_ref().map_or(0, |c| c.frames.load(Relaxed)),
                                    groups: c.as_ref().map_or(0, |c| c.groups.load(Relaxed)),
                                    fps: r.fps,
                                    kbps: r.kbps,
                                    dropped: b.dropped.get(&t.id).copied().unwrap_or(0),
                                    capture_to_publisher_ms: r.capture_ms,
                                    codec_string: c.and_then(|c| c.codec_string.lock().unwrap().clone()),
                                    subscribers: inner.demand.get(&b.path(&t.id)).map_or(0, |s| s.len()) as u32,
                                },
                            }
                        })
                        .collect(),
                    channels: b
                        .ad
                        .channels
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ChannelState {
                            index: i as u16,
                            info: c.clone(),
                        })
                        .collect(),
                    locks: b
                        .ad
                        .locks
                        .iter()
                        .enumerate()
                        .map(|(i, l)| LockState {
                            index: i as u16,
                            info: l.clone(),
                            holder: b.holders.get(&l.id).and_then(|s| inner.session_name(*s)),
                        })
                        .collect(),
                }
            })
            .collect();
        RoomState {
            room_id: self.id.clone(),
            broadcasts,
            participants,
        }
    }

    // --- sessions -------------------------------------------------------

    pub fn next_sid(&self) -> u32 {
        let mut inner = self.inner.lock().unwrap();
        loop {
            let sid = inner.next_sid;
            inner.next_sid = inner.next_sid.wrapping_add(1).max(1);
            if !inner.sessions.contains_key(&sid) {
                return sid;
            }
        }
    }

    pub fn join(&self, link: SessionLink) {
        let mut inner = self.inner.lock().unwrap();
        let participant = link.info.clone();
        inner.sessions.insert(participant.sid, link);
        self.emit(&mut inner, Event::ParticipantJoined { participant }, now_us());
    }

    /// The session ended: its locks are freed, its broadcasts stop (and are
    /// removed after the grace period unless an owner returns), its
    /// subscriptions end.
    pub fn leave(self: &Arc<Self>, sid: u32) {
        let now = now_us();
        let mut inner = self.inner.lock().unwrap();
        if !inner.sessions.contains_key(&sid) {
            return;
        }
        self.free_locks(&mut inner, sid);
        let owned: Vec<String> = inner.sessions[&sid].owned.iter().flatten().cloned().collect();
        for name in owned {
            self.stop_broadcast(&mut inner, &name, StopReason::Disconnected, now);
        }
        let paths: Vec<Path> = inner
            .demand
            .iter()
            .filter(|(_, s)| s.contains(&sid))
            .map(|(p, _)| p.clone())
            .collect();
        for p in paths {
            self.unsubscribe_locked(&mut inner, sid, &p);
        }
        let link = inner.sessions.remove(&sid).expect("checked above");
        self.emit(
            &mut inner,
            Event::ParticipantLeft {
                participant: link.info,
            },
            now,
        );
    }

    fn free_locks(&self, inner: &mut RoomInner, sid: u32) {
        let held: Vec<(String, String)> = inner
            .broadcasts
            .values()
            .flat_map(|b| {
                b.holders
                    .iter()
                    .filter(|(_, h)| **h == sid)
                    .map(|(l, _)| (b.name.clone(), l.clone()))
            })
            .collect();
        let from = inner.session_name(sid);
        for (bname, lock) in held {
            let b = inner.broadcasts.get_mut(&bname).expect("listed above");
            b.holders.remove(&lock);
            let path = b.path(&lock);
            let event = Event::LockChanged {
                lock: path,
                from: from.clone(),
                to: None,
                reason: LockChange::Left,
            };
            inner.notify_holder(&inner.broadcasts[&bname], &lock);
            self.emit(inner, event, now_us());
        }
    }

    /// The owner of `name` is gone; its tracks go offline. Removed after
    /// the grace period unless an owner returns.
    fn stop_broadcast(self: &Arc<Self>, inner: &mut RoomInner, name: &str, reason: StopReason, now: u64) {
        let Some(b) = inner.broadcasts.get_mut(name) else {
            return;
        };
        if let Some(owner) = b.owner.take() {
            if let Some(link) = inner.sessions.get_mut(&owner) {
                for o in link.owned.iter_mut().filter(|o| o.as_deref() == Some(name)) {
                    *o = None;
                }
            }
        }
        let b = inner.broadcasts.get_mut(name).expect("checked above");
        b.generation += 1;
        let generation = b.generation;
        for t in b.tracks.values() {
            t.groups.reset();
            t.datagrams.reset();
        }
        let offline: Vec<Path> = b
            .online
            .iter_mut()
            .filter(|(_, on)| **on)
            .map(|(id, on)| {
                *on = false;
                Path::new(name, id.clone())
            })
            .collect();
        for track in offline {
            let event = Event::TrackOffline {
                track,
                reason: "owner disconnected".into(),
            };
            self.emit(inner, event, now);
        }
        let event = Event::BroadcastStopped {
            broadcast: name.to_string(),
            reason,
        };
        self.emit(inner, event, now);
        if reason == StopReason::Unadvertised {
            self.remove_broadcast(inner, name);
            return;
        }
        let room = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(room.grace).await;
            let mut inner = room.inner.lock().unwrap();
            if inner
                .broadcasts
                .get(&name)
                .is_some_and(|b| b.owner.is_none() && b.generation == generation)
            {
                tracing::info!(room = %room.id, broadcast = %name, "owner did not return, removing broadcast");
                room.remove_broadcast(&mut inner, &name);
            }
        });
    }

    fn remove_broadcast(&self, inner: &mut RoomInner, name: &str) {
        let Some(b) = inner.broadcasts.remove(name) else {
            return;
        };
        let now = now_us();
        for t in &b.ad.tracks {
            self.emit(inner, Event::TrackUnpublished { track: b.path(&t.id) }, now);
        }
        for (lock, sid) in &b.holders {
            let event = Event::LockChanged {
                lock: b.path(lock),
                from: inner.session_name(*sid),
                to: None,
                reason: LockChange::Left,
            };
            self.emit(inner, event, now);
        }
        self.emit(
            inner,
            Event::BroadcastRemoved {
                broadcast: name.to_string(),
            },
            now,
        );
        self.bump_layout();
    }

    // --- publishing -----------------------------------------------------

    /// Makes `sid` the owner of `ad.broadcast` (new, returning after an
    /// outage, re-advertising, or taking over from another session) and
    /// applies what changed. The caller has checked `pub` and validated.
    pub fn advertise(self: &Arc<Self>, sid: u32, ad: Advertisement, build: Option<BuildInfo>) -> Advertised {
        let now = now_us();
        let mut inner = self.inner.lock().unwrap();
        let name = ad.broadcast.clone();
        let mut replaced = None;
        if let Some(prev) = inner.broadcasts.get(&name).and_then(|b| b.owner) {
            if prev != sid {
                replaced = Some(prev);
                self.stop_broadcast(&mut inner, &name, StopReason::Replaced, now);
            }
        }
        if !inner.broadcasts.contains_key(&name) {
            let index = inner.alloc_broadcast_index();
            inner.broadcasts.insert(
                name.clone(),
                Broadcast {
                    index,
                    name: name.clone(),
                    owner: None,
                    owner_index: 0,
                    generation: 0,
                    ad: Advertisement {
                        broadcast: name.clone(),
                        ..Default::default()
                    },
                    build: None,
                    tracks: HashMap::new(),
                    online: HashMap::new(),
                    holders: HashMap::new(),
                    dropped: HashMap::new(),
                    rates: HashMap::new(),
                },
            );
        }

        // The owner hop index: kept on re-advertise, new otherwise.
        let link = inner.sessions.get_mut(&sid).expect("advertising session is in the room");
        let owner_index = match link.owned.iter().position(|o| o.as_deref() == Some(name.as_str())) {
            Some(i) => i as u16,
            None => {
                link.owned.push(Some(name.clone()));
                (link.owned.len() - 1) as u16
            }
        };
        let session_name = link.info.session.clone();
        // Ahead of the demand and holder messages below, which refer to it.
        inner.send(
            sid,
            Control::AdvertiseOk {
                broadcast: name.clone(),
                index: owner_index,
                relay_us: now,
            },
        );

        let b = inner.broadcasts.get_mut(&name).expect("inserted above");
        let started = b.owner != Some(sid);
        if started {
            b.generation += 1;
        }
        b.owner = Some(sid);
        b.owner_index = owner_index;
        b.build = build.clone();
        let previous = std::mem::replace(&mut b.ad, ad);
        let mut events = Vec::new();
        if started {
            events.push(Event::BroadcastStarted {
                broadcast: name.clone(),
                owner: session_name,
                build,
            });
        }
        for gone in previous.tracks.iter().filter(|p| b.ad.track(&p.id).is_none()) {
            b.tracks.remove(&gone.id);
            b.online.remove(&gone.id);
            events.push(Event::TrackUnpublished {
                track: Path::new(&name, &gone.id),
            });
        }
        for t in b.ad.tracks.clone() {
            let path = Path::new(&name, &t.id);
            let kind_changed = previous.track(&t.id).is_some_and(|(_, p)| p.delivery() != t.delivery());
            if kind_changed || !b.tracks.contains_key(&t.id) {
                b.tracks.insert(t.id.clone(), RelayTrack::new());
            }
            match previous.track(&t.id) {
                None => events.push(Event::TrackPublished { track: path, info: t }),
                Some((_, p)) if *p != t => events.push(Event::TrackUpdated { track: path, info: t }),
                Some(_) => {}
            }
        }
        let gone_locks: Vec<String> = b
            .holders
            .keys()
            .filter(|l| b.ad.lock(l).is_none())
            .cloned()
            .collect();
        let mut freed = Vec::new();
        for l in gone_locks {
            if let Some(h) = b.holders.remove(&l) {
                freed.push((l, h));
            }
        }
        for (l, h) in freed {
            events.push(Event::LockChanged {
                lock: Path::new(&name, &l),
                from: inner.session_name(h),
                to: None,
                reason: LockChange::Left,
            });
        }
        for e in events {
            self.emit(&mut inner, e, now);
        }

        // Bring the owner up to date: demand and lock holders.
        let b = &inner.broadcasts[&name];
        let paths: Vec<Path> = b.ad.tracks.iter().map(|t| b.path(&t.id)).collect();
        let locks: Vec<String> = b.ad.locks.iter().map(|l| l.id.clone()).collect();
        for p in &paths {
            if inner.demand.get(p).is_some_and(|s| !s.is_empty()) {
                inner.notify_demand(p);
            }
        }
        for l in &locks {
            inner.notify_holder(b, l);
        }
        drop(inner);
        self.bump_layout();
        Advertised { owner_index, replaced }
    }

    /// Removes a broadcast its owner no longer publishes. False if `sid`
    /// does not own it.
    pub fn unadvertise(self: &Arc<Self>, sid: u32, name: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.broadcasts.get(name).and_then(|b| b.owner) != Some(sid) {
            return false;
        }
        self.stop_broadcast(&mut inner, name, StopReason::Unadvertised, now_us());
        true
    }

    /// A track edge from its owner, stamped `ts_us` on the owner's clock.
    /// Applied and re-emitted only if it changes something.
    pub fn owner_event(&self, sid: u32, event: Event, ts_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        let (Event::TrackOnline { track } | Event::TrackOffline { track, .. }) = &event else {
            return;
        };
        let offset = inner
            .sessions
            .get(&sid)
            .and_then(|s| s.clock.estimate())
            .map_or(0, |o| o.offset_us);
        let Some(b) = inner
            .broadcasts
            .get_mut(&track.broadcast)
            .filter(|b| b.owner == Some(sid))
        else {
            return;
        };
        if b.ad.track(&track.name).is_none() {
            return;
        }
        let online = matches!(event, Event::TrackOnline { .. });
        let was = b.online.insert(track.name.clone(), online).unwrap_or(false);
        if !online {
            if let Some(t) = b.tracks.get(&track.name) {
                t.groups.reset();
                t.datagrams.reset();
            }
        }
        if was != online {
            // On the relay's clock; never in the future.
            let ts_us = (ts_us as i64 - offset).clamp(0, now_us() as i64) as u64;
            self.emit(&mut inner, event, ts_us);
        }
    }

    pub fn owner_stats(&self, sid: u32, name: &str, tracks: Vec<tq_wire::OwnerTrackStats>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(b) = inner.broadcasts.get_mut(name).filter(|b| b.owner == Some(sid)) {
            for t in tracks {
                b.dropped.insert(t.id, t.dropped);
            }
        }
    }

    /// The track a session's group stream or datagram is for: the room's
    /// broadcast index, the track's id and its relay state.
    pub fn owner_track(&self, sid: u32, owner_index: u16, track: u16) -> Option<(u16, TrackInfo, RelayTrack)> {
        let inner = self.inner.lock().unwrap();
        let b = inner.owned(sid, owner_index)?;
        let info = b.ad.tracks.get(track as usize)?;
        Some((b.index, info.clone(), b.tracks.get(&info.id)?.clone()))
    }

    // --- subscribing ----------------------------------------------------

    /// Where a subscribed track is now, if it exists.
    pub fn resolve(&self, path: &Path) -> Option<Resolved> {
        let inner = self.inner.lock().unwrap();
        let b = inner.broadcasts.get(&path.broadcast)?;
        let (index, info) = b.ad.track(&path.name)?;
        Some(Resolved {
            broadcast: b.index,
            track: index,
            info: info.clone(),
            relay: b.tracks.get(&path.name)?.clone(),
        })
    }

    pub fn subscribe(&self, sid: u32, path: &Path) {
        let mut inner = self.inner.lock().unwrap();
        if inner.demand.entry(path.clone()).or_default().insert(sid) {
            inner.notify_demand(path);
            drop(inner);
            self.bump();
        }
    }

    pub fn unsubscribe(&self, sid: u32, path: &Path) {
        let mut inner = self.inner.lock().unwrap();
        if self.unsubscribe_locked(&mut inner, sid, path) {
            drop(inner);
            self.bump();
        }
    }

    fn unsubscribe_locked(&self, inner: &mut RoomInner, sid: u32, path: &Path) -> bool {
        let Some(set) = inner.demand.get_mut(path) else {
            return false;
        };
        if !set.remove(&sid) {
            return false;
        }
        if set.is_empty() {
            inner.demand.remove(path);
        }
        inner.notify_demand(path);
        true
    }

    /// Forwards a keyframe request to the track's owner, at most one per
    /// track per 500 ms.
    pub fn request_keyframe(&self, path: &Path, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        if inner
            .keyframe_requests
            .get(path)
            .is_some_and(|t| now - *t < KEYFRAME_REQUEST_INTERVAL)
        {
            return;
        }
        inner.keyframe_requests.insert(path.clone(), now);
        if let Some(owner) = inner.broadcasts.get(&path.broadcast).and_then(|b| b.owner) {
            inner.send(
                owner,
                Control::KeyframeRequest {
                    track: path.clone(),
                    reason: reason.to_string(),
                },
            );
        }
    }

    // --- locks ----------------------------------------------------------

    /// Takes a lock: `Ok(holder)`, or `Err((holder, reason))`. The caller
    /// has checked `lock:`; `force` is checked here, since it only matters
    /// when someone else holds the lock.
    pub fn claim(&self, sid: u32, path: &Path, force: bool) -> Result<String, (Option<String>, String)> {
        let mut inner = self.inner.lock().unwrap();
        let me = inner.session_name(sid).unwrap_or_default();
        let may_force = inner
            .sessions
            .get(&sid)
            .is_some_and(|s| s.perms().allows(Verb::Force, path));
        let Some(b) = inner.broadcasts.get_mut(&path.broadcast) else {
            return Err((None, format!("no broadcast '{}'", path.broadcast)));
        };
        if b.ad.lock(&path.name).is_none() {
            return Err((None, format!("no lock '{path}'")));
        }
        let previous = b.holders.get(&path.name).copied();
        match previous {
            Some(h) if h == sid => return Ok(me),
            Some(h) if !force || !may_force => {
                let holder = inner.session_name(h);
                let reason = if force {
                    "not allowed to force this lock"
                } else {
                    "held by another session; force to take over"
                };
                return Err((holder, reason.into()));
            }
            _ => {}
        }
        b.holders.insert(path.name.clone(), sid);
        let event = Event::LockChanged {
            lock: path.clone(),
            from: previous.and_then(|h| inner.session_name(h)),
            to: Some(me.clone()),
            reason: if previous.is_some() {
                LockChange::Forced
            } else {
                LockChange::Claimed
            },
        };
        inner.notify_holder(&inner.broadcasts[&path.broadcast], &path.name);
        self.emit(&mut inner, event, now_us());
        tracing::info!(room = %self.id, lock = %path, session = %me, "lock claimed");
        Ok(me)
    }

    /// Gives up a lock if `sid` holds it.
    pub fn release(&self, sid: u32, path: &Path) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(b) = inner.broadcasts.get_mut(&path.broadcast) else {
            return false;
        };
        if b.holders.get(&path.name) != Some(&sid) {
            return false;
        }
        b.holders.remove(&path.name);
        let event = Event::LockChanged {
            lock: path.clone(),
            from: inner.session_name(sid),
            to: None,
            reason: LockChange::Released,
        };
        inner.notify_holder(&inner.broadcasts[&path.broadcast], &path.name);
        self.emit(&mut inner, event, now_us());
        true
    }

    // --- datagrams ------------------------------------------------------

    /// A channel message or heartbeat from `sid` (subscriber hop): checks
    /// it and returns the owner's session and the datagram rewritten for
    /// the owner hop, or why it was dropped.
    pub fn route_to_owner(&self, sid: u32, mut data: Vec<u8>) -> Result<(Session, Bytes), &'static str> {
        let (_, broadcast, id) = tq_wire::peek_route(&data).ok_or("malformed")?;
        let heartbeat = data[0] == tq_wire::DG_HEARTBEAT;
        let inner = self.inner.lock().unwrap();
        let b = inner.broadcast_at(broadcast).ok_or("unknown broadcast")?;
        let owner = inner
            .sessions
            .get(&b.owner.ok_or("broadcast offline")?)
            .ok_or("broadcast offline")?;
        let lock = if heartbeat {
            Some(b.ad.locks.get(id as usize).ok_or("unknown lock")?.id.as_str())
        } else {
            let channel = b.ad.channels.get(id as usize).ok_or("unknown channel")?;
            let me = inner.sessions.get(&sid).ok_or("unknown session")?;
            if !me.perms().allows(Verb::Write, &b.path(&channel.id)) {
                return Err("not allowed to write");
            }
            channel.lock.as_deref()
        };
        if let Some(lock) = lock {
            if b.holders.get(lock) != Some(&sid) {
                return Err("lock not held");
            }
        }
        tq_wire::set_route(&mut data, sid, b.owner_index);
        Ok((owner.session.clone(), Bytes::from(data)))
    }

    /// An ack or echo from an owner (owner hop): returns the target's
    /// session and the datagram rewritten for the subscriber hop.
    pub fn route_to_sender(&self, sid: u32, mut data: Vec<u8>) -> Option<(Session, Bytes)> {
        let (target, owner_index, _) = tq_wire::peek_route(&data)?;
        let inner = self.inner.lock().unwrap();
        let b = inner.owned(sid, owner_index)?;
        let session = inner.sessions.get(&target)?.session.clone();
        tq_wire::set_route(&mut data, target, b.index);
        Some((session, Bytes::from(data)))
    }

    // --- clocks and links -----------------------------------------------

    pub fn pong(&self, sid: u32, offset_us: i64, rtt_us: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.sessions.get_mut(&sid) {
            s.rtt_ms = Some(rtt_us as f64 / 1000.0);
            s.clock.add(offset_us, rtt_us);
        }
    }

    pub fn set_quic(&self, sid: u32, quic: QuicStats) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.sessions.get_mut(&sid) {
            s.quic = Some(quic);
        }
    }

    /// Recomputes per-track rates; returns whether anything is published.
    fn tick_stats(&self, secs: f64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        for b in inner.broadcasts.values_mut() {
            for (id, t) in &b.tracks {
                let c = &t.counters;
                let r = b.rates.entry(id.clone()).or_default();
                let (frames, bytes) = (c.frames.load(Relaxed), c.bytes.load(Relaxed));
                r.fps = frames.saturating_sub(r.frames) as f64 / secs;
                r.kbps = bytes.saturating_sub(r.bytes) as f64 * 8.0 / 1000.0 / secs;
                (r.frames, r.bytes) = (frames, bytes);
                let n = c.capture_lat_n.swap(0, Relaxed);
                let sum = c.capture_lat_sum_us.swap(0, Relaxed);
                r.capture_ms = (n > 0).then(|| sum as f64 / n as f64 / 1000.0);
            }
        }
        !inner.broadcasts.is_empty()
    }
}

/// Live numbers once a second, while the room exists.
async fn stats_ticker(room: Weak<Room>) {
    let mut t = tokio::time::interval(STATS_INTERVAL);
    let mut last = Instant::now();
    loop {
        t.tick().await;
        let Some(room) = room.upgrade() else { return };
        let secs = last.elapsed().as_secs_f64().max(1e-3);
        last = Instant::now();
        if room.tick_stats(secs) {
            room.bump();
        }
    }
}
