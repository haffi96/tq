//! The owner side: a broadcast's tracks, its incoming channel messages,
//! its locks' holders and watchdogs. A [`Publication`] lives as long as the
//! program does; each [`crate::Session`] that publishes it carries it to
//! the relay.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, watch};
use tq_transport::{dgtrack::DatagramTrack, track::Track, wt};
use tq_wire::{
    now_us, AckStatus, Advertisement, Datagram, Delivery, Event, FrameHeader, MediaFrame, Path, TrackKind,
    ECHO_TRIPPED, FLAG_KEY,
};

use crate::watchdog::{self, Edge, WatchdogStatus};

/// Messages queued for the app before new ones are refused.
const INBOX: usize = 256;
const WATCHDOG_TICK: Duration = Duration::from_millis(20);
/// Edges in flight to the relay before the session restates everything.
const EDGES: usize = 64;

/// How a session publishes.
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// Uplink drop policy: a frame older than this since it was ready is
    /// dropped with the rest of its group.
    pub max_lag: Duration,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            max_lag: Duration::from_millis(400),
        }
    }
}

pub(crate) enum LocalTrack {
    Groups(Arc<Track>),
    Datagrams(Arc<DatagramTrack>),
}

/// Where a data track's next message goes.
#[derive(Default)]
struct WriterState {
    group: u64,
    index: u32,
    window_start: Option<Instant>,
    seq: u32,
}

/// Who holds a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub sid: u32,
    pub session: String,
    /// The relay's number for this claim: a new claim by the same session
    /// has a new one.
    pub claim: Option<u64>,
}

/// The session a publication is currently published on, for replies.
#[derive(Clone)]
pub(crate) struct Link {
    pub session: wt::Session,
    pub owner_index: u16,
}

pub struct Publication {
    ad: Advertisement,
    pub(crate) tracks: Vec<LocalTrack>,
    writers: Vec<Mutex<WriterState>>,
    pub(crate) dropped: Vec<AtomicU64>,
    online: Mutex<HashMap<String, bool>>,
    edges: broadcast::Sender<(Event, u64)>,
    holders: watch::Sender<HashMap<String, Option<Holder>>>,
    /// Each lock's holder as last told by the relay. Unlike `holders`, kept
    /// through outages: the relay restates holders on reconnect, and a
    /// restatement (same claim number) is not a claim.
    known_holders: Mutex<HashMap<String, Option<Holder>>>,
    claims: broadcast::Sender<(String, Holder)>,
    demand: watch::Sender<HashMap<String, u32>>,
    watchdogs: HashMap<String, (Mutex<watchdog::State>, watch::Sender<WatchdogStatus>)>,
    keyframes: broadcast::Sender<String>,
    inbox: mpsc::Sender<Message>,
    /// Latest applied seq per (channel, sender).
    last_seq: Mutex<HashMap<(u16, u32), u32>>,
    link: Mutex<Option<Link>>,
}

impl Publication {
    /// A publication of `ad`, and the inbox its channel messages arrive in.
    pub fn new(ad: Advertisement) -> anyhow::Result<(Arc<Self>, Inbox)> {
        ad.validate().map_err(anyhow::Error::msg)?;
        let tracks = ad
            .tracks
            .iter()
            .map(|t| match t.is_datagram() {
                true => LocalTrack::Datagrams(DatagramTrack::new()),
                false => LocalTrack::Groups(Track::new()),
            })
            .collect();
        let watchdogs = ad
            .locks
            .iter()
            .filter_map(|l| {
                let cfg = l.watchdog.clone()?;
                Some((
                    l.id.clone(),
                    (
                        Mutex::new(watchdog::State::new(cfg)),
                        watch::channel(WatchdogStatus::default()).0,
                    ),
                ))
            })
            .collect();
        let (inbox, rx) = mpsc::channel(INBOX);
        let p = Arc::new(Self {
            writers: ad.tracks.iter().map(|_| Default::default()).collect(),
            dropped: ad.tracks.iter().map(|_| AtomicU64::new(0)).collect(),
            online: Mutex::new(ad.tracks.iter().map(|t| (t.id.clone(), false)).collect()),
            holders: watch::channel(ad.locks.iter().map(|l| (l.id.clone(), None)).collect()).0,
            known_holders: Mutex::default(),
            claims: broadcast::channel(16).0,
            demand: watch::channel(HashMap::new()).0,
            edges: broadcast::channel(EDGES).0,
            keyframes: broadcast::channel(16).0,
            last_seq: Default::default(),
            link: Mutex::new(None),
            tracks,
            watchdogs,
            inbox,
            ad,
        });
        if !p.watchdogs.is_empty() {
            tokio::spawn(watchdog_ticker(Arc::downgrade(&p)));
        }
        Ok((p, Inbox { rx }))
    }

    pub fn name(&self) -> &str {
        &self.ad.broadcast
    }

    pub fn advertisement(&self) -> &Advertisement {
        &self.ad
    }

    /// The group cache of a video track: push the source's frames here.
    /// Frames need their group and index; the session stamps the rest.
    pub fn video(&self, id: &str) -> Option<Arc<Track>> {
        let (i, t) = self.ad.track(id)?;
        match (&self.tracks[i as usize], t.kind) {
            (LocalTrack::Groups(g), TrackKind::Video) => Some(g.clone()),
            _ => None,
        }
    }

    /// A writer for a data track.
    pub fn data(self: &Arc<Self>, id: &str) -> Option<DataWriter> {
        let (i, t) = self.ad.track(id)?;
        t.data.as_ref()?;
        Some(DataWriter {
            p: self.clone(),
            index: i,
        })
    }

    /// Writes one message on data track `index`.
    fn write(&self, index: u16, payload: Bytes, capture_us: u64) {
        let Some(d) = self.ad.tracks.get(index as usize).and_then(|t| t.data.as_ref()) else {
            return;
        };
        let publisher_us = now_us();
        let mut w = self.writers[index as usize].lock().unwrap();
        match &self.tracks[index as usize] {
            LocalTrack::Datagrams(dg) => {
                w.seq = w.seq.wrapping_add(1);
                dg.push(
                    Datagram::TrackFrame {
                        // The session writes its broadcast index.
                        broadcast: 0,
                        track: index,
                        seq: w.seq,
                        flags: FLAG_KEY,
                        capture_us,
                        publisher_us,
                        relay_us: 0,
                        payload,
                    }
                    .encode(),
                );
            }
            LocalTrack::Groups(track) => {
                let now = Instant::now();
                let window = Duration::from_millis(d.window_ms.unwrap_or(1000) as u64);
                let new_group = match d.delivery {
                    Delivery::Stream => w.window_start.is_none_or(|t| now - t >= window),
                    _ => true,
                };
                if new_group {
                    w.group += 1;
                    w.index = 0;
                    w.window_start = Some(now);
                } else {
                    w.index += 1;
                }
                let frame = MediaFrame {
                    broadcast: 0,
                    track: index,
                    group: w.group,
                    codec: d.encoding.as_str().into(),
                    header: FrameHeader {
                        index: w.index,
                        flags: if w.index == 0 { FLAG_KEY } else { 0 },
                        capture_us,
                        publisher_us,
                        relay_us: 0,
                        frame_id: 0,
                    },
                    payload,
                };
                track.push(frame);
            }
        }
    }

    /// Marks a track's source as producing or not; the room hears it as an
    /// event. Data tracks start offline too.
    pub fn set_online(&self, id: &str, online: bool, reason: &str) {
        let mut map = self.online.lock().unwrap();
        let Some(was) = map.get_mut(id) else { return };
        if *was == online {
            return;
        }
        *was = online;
        if !online {
            if let Some((i, _)) = self.ad.track(id) {
                match &self.tracks[i as usize] {
                    LocalTrack::Groups(g) => g.reset(),
                    LocalTrack::Datagrams(d) => d.reset(),
                }
            }
        }
        let track = Path::new(self.name(), id);
        let event = match online {
            true => Event::TrackOnline { track },
            false => Event::TrackOffline {
                track,
                reason: reason.to_string(),
            },
        };
        // Nobody listening (not connected) is fine: a session restates the
        // current values when it publishes.
        let _ = self.edges.send((event, now_us()));
    }

    /// Adds to a track's count of frames dropped before the relay (shown
    /// in the room state).
    pub fn add_dropped(&self, id: &str, n: u64) {
        if let Some((i, _)) = self.ad.track(id) {
            self.dropped[i as usize].fetch_add(n, Relaxed);
        }
    }

    /// Who holds each lock, as the relay says.
    pub fn holders(&self) -> watch::Receiver<HashMap<String, Option<Holder>>> {
        self.holders.subscribe()
    }

    /// Subscribers per track. A track nobody subscribes to is not sent;
    /// its source may pause.
    /// Each new claim of one of this broadcast's locks: `(lock, holder)`,
    /// one per claim even when they come quickly (a release and a claim by
    /// the same session are two changes). The relay restating holders after
    /// an outage is not a claim.
    pub fn claims(&self) -> broadcast::Receiver<(String, Holder)> {
        self.claims.subscribe()
    }

    pub fn demand(&self) -> watch::Receiver<HashMap<String, u32>> {
        self.demand.subscribe()
    }

    /// The watchdog of a lock that has one.
    pub fn watchdog(&self, lock: &str) -> Option<watch::Receiver<WatchdogStatus>> {
        self.watchdogs.get(lock).map(|(_, tx)| tx.subscribe())
    }

    /// Keyframe requests from subscribers, by track id. The relay limits
    /// them to one per track per 500 ms.
    pub fn keyframe_requests(&self) -> broadcast::Receiver<String> {
        self.keyframes.subscribe()
    }

    // --- for the session ------------------------------------------------

    pub(crate) fn current_edges(&self) -> Vec<Event> {
        let online = self.online.lock().unwrap();
        self.ad
            .tracks
            .iter()
            .map(|t| {
                let track = Path::new(self.name(), &t.id);
                if online.get(&t.id).copied().unwrap_or(false) {
                    Event::TrackOnline { track }
                } else {
                    Event::TrackOffline {
                        track,
                        reason: "source not producing".into(),
                    }
                }
            })
            .collect()
    }

    pub(crate) fn subscribe_edges(&self) -> broadcast::Receiver<(Event, u64)> {
        self.edges.subscribe()
    }

    pub(crate) fn attach(&self, link: Link) {
        *self.link.lock().unwrap() = Some(link);
    }

    /// The session carrying this publication ended.
    pub(crate) fn detach(&self, session: &wt::Session) {
        let mut link = self.link.lock().unwrap();
        if link.as_ref().is_some_and(|l| &l.session == session) {
            *link = None;
            drop(link);
            self.demand.send_replace(HashMap::new());
            self.holders.send_modify(|h| h.values_mut().for_each(|v| *v = None));
            for (lock, (state, tx)) in &self.watchdogs {
                let mut s = state.lock().unwrap();
                let edge = s.connection_lost();
                self.publish_edge(lock, edge, &s.status, tx);
            }
        }
    }

    /// Trips a lock's watchdog from the app (a remote stop, say). It clears
    /// like any trip: on the holder's next accepted message.
    pub fn trip(&self, lock: &str, reason: &str) {
        if let Some((state, tx)) = self.watchdogs.get(lock) {
            let mut s = state.lock().unwrap();
            let edge = s.trip_by_app(reason);
            self.publish_edge(lock, edge, &s.status, tx);
        }
    }

    pub(crate) fn set_demand(&self, track: &str, subscribers: u32) {
        self.demand.send_modify(|d| {
            d.insert(track.to_string(), subscribers);
        });
    }

    pub(crate) fn request_keyframe(&self, track: &str) {
        let _ = self.keyframes.send(track.to_string());
    }

    pub(crate) fn set_holder(&self, lock: &str, holder: Option<Holder>) {
        let sid = holder.as_ref().map(|h| h.sid);
        let previous = self
            .known_holders
            .lock()
            .unwrap()
            .insert(lock.to_string(), holder.clone())
            .flatten();
        if let Some(h) = &holder {
            if previous.as_ref() != Some(h) {
                let _ = self.claims.send((lock.to_string(), h.clone()));
            }
        }
        self.holders.send_modify(|h| {
            h.insert(lock.to_string(), holder);
        });
        // Sequence numbers restart with a new holder.
        let channels: Vec<u16> = self
            .ad
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| c.lock.as_deref() == Some(lock))
            .map(|(i, _)| i as u16)
            .collect();
        self.last_seq.lock().unwrap().retain(|(c, _), _| !channels.contains(c));
        if let Some((state, tx)) = self.watchdogs.get(lock) {
            let mut s = state.lock().unwrap();
            let edge = s.set_holder(sid);
            self.publish_edge(lock, edge, &s.status, tx);
        }
    }

    fn publish_edge(&self, lock: &str, edge: Option<Edge>, status: &WatchdogStatus, tx: &watch::Sender<WatchdogStatus>) {
        tx.send_replace(status.clone());
        let Some(edge) = edge else { return };
        tracing::info!(broadcast = %self.name(), lock, ?edge, "watchdog");
        if let Some((index, _)) = self.ad.track(".events") {
            let event = match edge {
                Edge::Tripped(reason) => serde_json::json!({ "type": "watchdog_tripped", "lock": lock, "reason": reason }),
                Edge::Cleared => serde_json::json!({ "type": "watchdog_cleared", "lock": lock }),
            };
            self.write(index, serde_json::to_vec(&event).expect("JSON serializes").into(), now_us());
        }
    }

    fn tick_watchdogs(&self) {
        for (lock, (state, tx)) in &self.watchdogs {
            let mut s = state.lock().unwrap();
            if let Some(edge) = s.tick() {
                self.publish_edge(lock, Some(edge), &s.status, tx);
            }
        }
    }

    /// A heartbeat on lock `lock` from `sid`: its echo.
    pub(crate) fn heartbeat(&self, lock: u16, sid: u32, seq: u32, sent_us: u64) -> Option<Datagram> {
        let id = &self.ad.locks.get(lock as usize)?.id;
        let tripped = match self.watchdogs.get(id) {
            Some((state, _)) => {
                let mut s = state.lock().unwrap();
                s.heartbeat(sid);
                s.status.tripped
            }
            None => false,
        };
        let link = self.link.lock().unwrap().clone()?;
        Some(Datagram::Echo {
            sid,
            broadcast: link.owner_index,
            lock,
            seq,
            echo_us: sent_us,
            flags: if tripped { ECHO_TRIPPED } else { 0 },
        })
    }

    /// A channel message from `sid`: delivered to the inbox unless stale
    /// (acked `superseded`) or the inbox is full (acked `rejected`).
    pub(crate) fn message(self: &Arc<Self>, channel: u16, sid: u32, seq: u32, sent_us: u64, payload: Bytes) {
        let Some(info) = self.ad.channels.get(channel as usize) else {
            return;
        };
        let Some(link) = self.link.lock().unwrap().clone() else {
            return;
        };
        let msg = Message {
            channel: info.id.clone(),
            lock: info.lock.clone(),
            from_sid: sid,
            seq,
            sent_us,
            payload,
            reply: Replier {
                p: Arc::downgrade(self),
                link,
                channel,
            },
        };
        {
            let mut last = self.last_seq.lock().unwrap();
            let entry = last.entry((channel, sid));
            if let std::collections::hash_map::Entry::Occupied(e) = &entry {
                if seq <= *e.get() {
                    msg.ack(AckStatus::Superseded, "older than last applied");
                    return;
                }
            }
            entry.insert_entry(seq);
        }
        if let Some((state, _)) = info.lock.as_ref().and_then(|l| self.watchdogs.get(l)) {
            state.lock().unwrap().message(sid);
        }
        if let Err(mpsc::error::TrySendError::Full(msg)) = self.inbox.try_send(msg) {
            msg.ack(AckStatus::Rejected, "inbox full");
        }
    }

    fn accepted(&self, channel: u16, sid: u32) {
        let Some(lock) = self.ad.channels.get(channel as usize).and_then(|c| c.lock.as_ref()) else {
            return;
        };
        if let Some((state, tx)) = self.watchdogs.get(lock) {
            let mut s = state.lock().unwrap();
            let edge = s.accepted(sid);
            self.publish_edge(lock, edge, &s.status, tx);
        }
    }
}

async fn watchdog_ticker(p: Weak<Publication>) {
    let mut t = tokio::time::interval(WATCHDOG_TICK);
    loop {
        t.tick().await;
        let Some(p) = p.upgrade() else { return };
        p.tick_watchdogs();
    }
}

/// Writes a data track's messages.
#[derive(Clone)]
pub struct DataWriter {
    p: Arc<Publication>,
    index: u16,
}

impl DataWriter {
    /// Sends a message captured now.
    pub fn send(&self, payload: impl Into<Bytes>) {
        self.send_at(payload, now_us());
    }

    pub fn send_json<T: serde::Serialize + ?Sized>(&self, value: &T) {
        self.send(serde_json::to_vec(value).expect("JSON serializes"));
    }

    /// Sends a message captured at `capture_us` (this machine's clock).
    pub fn send_at(&self, payload: impl Into<Bytes>, capture_us: u64) {
        self.p.write(self.index, payload.into(), capture_us);
    }
}

/// Channel messages sent into a publication.
pub struct Inbox {
    rx: mpsc::Receiver<Message>,
}

impl Inbox {
    pub async fn recv(&mut self) -> Option<Message> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Message> {
        self.rx.try_recv().ok()
    }
}

/// One message on one of a publication's channels. Ack it once: the
/// sender measures its round trip from the ack, and an accepted message
/// re-arms its lock's watchdog.
pub struct Message {
    pub channel: String,
    /// The lock the channel belongs to, if any.
    pub lock: Option<String>,
    pub from_sid: u32,
    pub seq: u32,
    /// On the sender's clock.
    pub sent_us: u64,
    pub payload: Bytes,
    reply: Replier,
}

struct Replier {
    p: Weak<Publication>,
    link: Link,
    channel: u16,
}

impl Message {
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    pub fn ack(&self, status: AckStatus, detail: &str) {
        let r = &self.reply;
        if status == AckStatus::Ok {
            if let Some(p) = r.p.upgrade() {
                p.accepted(r.channel, self.from_sid);
            }
        }
        let _ = r.link.session.send_datagram(
            Datagram::Ack {
                sid: self.from_sid,
                broadcast: r.link.owner_index,
                channel: r.channel,
                seq: self.seq,
                echo_us: self.sent_us,
                status,
                detail: detail.to_string(),
            }
            .encode(),
        );
    }

    /// Applied: acks `ok`.
    pub fn accept(&self) {
        self.ack(AckStatus::Ok, "");
    }

    pub fn reject(&self, detail: &str) {
        self.ack(AckStatus::Rejected, detail);
    }

    pub fn malformed(&self, detail: &str) {
        self.ack(AckStatus::Malformed, detail);
    }
}
