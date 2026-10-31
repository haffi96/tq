//! The relay session: the native counterpart of `web/src/worker.ts`, on the
//! tq client SDK ([`tq::Session`]).
//!
//! Runs on a tokio runtime next to the UI. It connects (and reconnects) to
//! the relay, follows the room, subscribes to every broadcast's video
//! tracks (each into its own decoder thread, [`crate::feed`]) and to their
//! `.status` and `.events` data tracks, and drives the `drive` lock and
//! channel of the broadcast being driven. The UI hears about connection,
//! room and teleop changes through [`Event`]s and acts through [`Shared`].

use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use futures::channel::mpsc::UnboundedSender;
use tokio::sync::broadcast::{self, error::RecvError};
use tq::{Client, Conn, Hold, Reply, Session, Token};
use tq_wire::{BroadcastState, BuildInfo, Control, Path, Perms, RoomEvent, RoomState, TrackInfo, TrackKind};

use crate::{
    feed::{Feed, Job},
    stats::Win,
};

const HEARTBEAT: Duration = Duration::from_millis(100);
const TELEOP: Duration = Duration::from_millis(50);
/// How long a replayed `.events` message waits for the clock offsets.
const CLOCK_WAIT: Duration = Duration::from_secs(1);

/// The lock (and the channel behind it) an operator drives with.
pub const DRIVE: &str = "drive";
/// The channel that stops a robot, whoever drives it.
pub const STOP: &str = "stop";
/// Remote stops go as datagrams, which can be lost: each is sent again this
/// often until the robot acks one (it treats repeats as one press)...
const STOP_RETRY: Duration = Duration::from_millis(50);
/// ...for this long.
const STOP_GIVE_UP: Duration = Duration::from_secs(2);
/// A broadcast's latest status (JSON; teleop's state under `teleop`).
pub const STATUS: &str = ".status";
/// A broadcast's own event log (JSON stream).
pub const EVENTS: &str = ".events";

#[derive(Debug, Clone)]
pub struct Config {
    /// Relay HTTP base, for `/api/info` and dev tokens.
    pub http: url::Url,
    /// `host:port` of the relay's QUIC endpoint; default: the HTTP host and
    /// the port from `/api/info`.
    pub relay: Option<String>,
    pub room: String,
    /// Token preset: `operator` or `viewer`.
    pub preset: String,
    pub token: Option<String>,
    /// Signs a token locally instead of asking the relay for a dev token.
    pub secret: Option<String>,
    pub log_stats: bool,
    /// This app's build, sent in `hello`.
    pub build: BuildInfo,
}

impl Config {
    /// Shows and runs the operator controls.
    pub fn operator(&self) -> bool {
        self.preset == "operator"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Closed,
}

/// Session -> UI. Low rate, so the large variants are not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    Status(ConnState, Option<String>),
    /// The relay's welcome: this session's id and the relay's build.
    Connected {
        session: String,
        relay: BuildInfo,
    },
    /// The room's newest state.
    Room(Arc<RoomState>),
    /// One of the room's events.
    RoomEvent(RoomEvent),
    /// Control messages the UI shows (link stats, relay errors).
    Control(Control),
    /// A broadcast's newest `.status`.
    BroadcastStatus(String, serde_json::Value),
    /// A line for the events log.
    Log(String),
    /// A line for the events log that happened at a given time (Unix µs,
    /// this machine's clock).
    LogAt(u64, String),
    /// A feed got a new picture (coalesced: at most one pending).
    Frame,
}

/// What the operator is asking for; sent at 20 Hz while `active`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Drive {
    pub throttle: f64,
    pub steer: f64,
    pub active: bool,
}

#[derive(Default)]
pub struct Teleop {
    pub ack_rtt: Win,
    pub heartbeat_rtt: Win,
    pub acks: HashMap<String, u64>,
    pub last_ack_detail: Option<String>,
    /// The drive lock's watchdog as seen in heartbeat echoes.
    pub tripped: Option<bool>,
}

/// State shared by the session, the decoder threads and the UI.
pub struct Shared {
    pub config: Config,
    pub teleop: Mutex<Teleop>,
    pub drive: Mutex<Drive>,
    /// One feed per video track, in the order they appeared. Kept across
    /// reconnects so the UI tiles stay put.
    pub feeds: Mutex<Vec<Arc<Feed>>>,
    client: Client,
    /// The drive lock, while this app holds it.
    hold: Mutex<Option<Hold>>,
    rt: tokio::runtime::Handle,
    events: UnboundedSender<Event>,
    frame_pending: AtomicBool,
}

/// The broadcast an operator drives: the first with a `drive` lock.
pub fn drive_broadcast(room: &RoomState) -> Option<&BroadcastState> {
    room.broadcasts.iter().find(|b| b.lock(DRIVE).is_some())
}

/// A track as the tiles name it: its label, or `broadcast/track`.
pub fn track_label(broadcast: &str, info: &TrackInfo) -> String {
    if info.label.is_empty() {
        format!("{broadcast}/{}", info.id)
    } else {
        info.label.clone()
    }
}

impl Shared {
    pub fn feeds(&self) -> Vec<Arc<Feed>> {
        self.feeds.lock().unwrap().clone()
    }

    /// Wakes the UI for a new picture; repeated calls before the UI has
    /// rendered collapse into one.
    pub fn frame_ready(&self) {
        use std::sync::atomic::Ordering;
        if !self.frame_pending.swap(true, Ordering::AcqRel) {
            let _ = self.events.unbounded_send(Event::Frame);
        }
    }

    /// The UI took the pending frame wake-up.
    pub fn frame_taken(&self) {
        self.frame_pending.store(false, std::sync::atomic::Ordering::Release);
    }

    pub fn session(&self) -> Option<Session> {
        self.client.session()
    }

    pub fn request_keyframe(&self, track: &Path, reason: &str) {
        if let Some(s) = self.session() {
            s.request_keyframe(track, reason);
        }
    }

    pub fn relay_rtt_ms(&self) -> Option<f64> {
        self.session()?.relay_rtt_ms()
    }

    /// Bound on the error of `broadcast`'s owner clock -> this clock (both
    /// hops), ms.
    pub fn sync_error_ms(&self, broadcast: &str) -> Option<f64> {
        Some(self.session()?.owner_offset(broadcast)?.error_us as f64 / 1000.0)
    }

    /// The worst [`Self::sync_error_ms`] over the room's broadcasts.
    pub fn worst_sync_error_ms(&self) -> Option<f64> {
        let s = self.session()?;
        let names: Vec<String> = s.room().borrow().broadcasts.iter().map(|b| b.name.clone()).collect();
        names
            .iter()
            .filter_map(|b| s.owner_offset(b))
            .map(|o| o.error_us as f64 / 1000.0)
            .max_by(f64::total_cmp)
    }

    /// Claims the drive lock (taking it from its holder with `force`) and
    /// heartbeats it while held. Claiming it while holding it claims it
    /// anew, which re-arms a robot after a remote stop.
    pub fn claim(self: &Arc<Self>, force: bool) {
        let Some((session, lock)) = self.drive_target() else {
            return;
        };
        let previous = self.hold.lock().unwrap().take();
        let shared = self.clone();
        self.rt.spawn(async move {
            if let Some(h) = previous.filter(|h| h.is_held()) {
                let _ = h.release().await;
            }
            let line = match session.hold(lock.clone(), force, HEARTBEAT).await {
                Ok(Ok(hold)) => {
                    *shared.hold.lock().unwrap() = Some(hold);
                    format!("Claim granted: you are driving {}", lock.broadcast)
                }
                Ok(Err(refused)) => format!("Claim refused: {}", refused.reason),
                Err(e) => format!("Claim failed: {e:#}"),
            };
            shared.event(Event::Log(line));
        });
    }

    /// Releases the drive lock; the robot's dead-man trips at once.
    pub fn release(self: &Arc<Self>) {
        let Some((session, lock)) = self.drive_target() else {
            return;
        };
        let hold = self.hold.lock().unwrap().take();
        let shared = self.clone();
        self.rt.spawn(async move {
            let r = match hold {
                Some(h) if h.is_held() => h.release().await,
                _ => session.release(lock).await,
            };
            if let Err(e) = r {
                shared.event(Event::Log(format!("Release failed: {e:#}")));
            }
        });
    }

    /// Remote stop: every robot in the room safe-stops, whoever drives it, and
    /// stays stopped until its drive lock is claimed again. This app lets
    /// go of its own drive lock too.
    pub fn remote_stop(self: &Arc<Self>) {
        let targets: Vec<Path> = self
            .client
            .room()
            .borrow()
            .broadcasts
            .iter()
            .filter(|b| b.channel(STOP).is_some())
            .map(|b| Path::new(&b.name, STOP))
            .collect();
        if targets.is_empty() {
            self.event(Event::Log("STOP: no broadcast with a stop channel".into()));
        }
        for channel in targets {
            self.rt.spawn(send_remote_stop(self.clone(), channel));
        }
        if let Some(hold) = self.hold.lock().unwrap().take() {
            self.rt.spawn(async move {
                let _ = hold.release().await;
            });
        }
    }

    /// One message on the drive channel, outside the 20 Hz stream.
    pub fn send_drive(&self, command: &str, args: serde_json::Value) {
        let Some((session, lock)) = self.drive_target() else {
            return;
        };
        let msg = serde_json::json!({ "command": command, "args": args });
        if let Err(e) = session.channel(lock).send_json(&msg) {
            self.event(Event::Log(format!("Send {command}: {e:#}")));
        }
    }

    /// The session and the drive lock, or why not (logged).
    fn drive_target(&self) -> Option<(Session, Path)> {
        let Some(session) = self.session() else {
            self.event(Event::Log("Not connected".into()));
            return None;
        };
        let lock = drive_broadcast(&session.room().borrow()).map(|b| Path::new(&b.name, DRIVE));
        match lock {
            Some(lock) => Some((session, lock)),
            None => {
                self.event(Event::Log("Nothing to drive: no broadcast has a drive lock".into()));
                None
            }
        }
    }

    fn event(&self, e: Event) {
        let _ = self.events.unbounded_send(e);
    }

    /// The feed of a video track, made on first sight.
    fn feed_for(self: &Arc<Self>, path: &Path, label: String) -> Arc<Feed> {
        let mut feeds = self.feeds.lock().unwrap();
        if let Some(f) = feeds.iter().find(|f| f.path == *path) {
            return f.clone();
        }
        let f = Feed::spawn(path.clone(), label, Arc::downgrade(self));
        feeds.push(f.clone());
        f
    }
}

/// Starts the session on `rt`. Events for the UI arrive on `events`.
pub fn start(rt: &tokio::runtime::Runtime, config: Config, events: UnboundedSender<Event>) -> Arc<Shared> {
    let _guard = rt.enter();
    let client = Client::new({
        let config = config.clone();
        move || options(config.clone())
    });
    let shared = Arc::new(Shared {
        config,
        teleop: Mutex::default(),
        drive: Mutex::default(),
        feeds: Mutex::default(),
        client: client.clone(),
        hold: Mutex::default(),
        rt: rt.handle().clone(),
        events,
        frame_pending: Default::default(),
    });
    rt.spawn(follow_conn(shared.clone(), client.conn()));
    rt.spawn(follow_room(shared.clone(), client.clone()));
    rt.spawn(forward(shared.clone(), client.events(), |e| Some(Event::RoomEvent(e))));
    rt.spawn(forward(shared.clone(), client.control(), |m| match m {
        Control::LinkStats { .. } | Control::Error { .. } => Some(Event::Control(m)),
        _ => None,
    }));
    rt.spawn(read_replies(shared.clone(), client.replies()));
    if shared.config.operator() {
        rt.spawn(send_teleop(shared.clone()));
    }
    shared
}

/// Options for each connection attempt: the relay found over HTTP(S), and
/// a token (given, signed here, or from a dev relay).
async fn options(c: Config) -> anyhow::Result<tq::Options> {
    let sub = format!("app-{}", c.preset);
    let token = match (&c.token, &c.secret) {
        (Some(t), _) => Token::Given(t.clone()),
        (None, Some(secret)) => Token::Signed {
            secret: secret.clone(),
            perms: Perms::preset(&c.preset).with_context(|| format!("unknown preset '{}'", c.preset))?,
            sub,
            ttl: Duration::from_secs(12 * 3600),
        },
        (None, None) => Token::Dev {
            preset: c.preset.clone(),
            sub,
        },
    };
    tq::Options::discover(&c.http, c.relay.as_deref(), &c.room, &token, c.build.clone()).await
}

/// Tells the UI about the connection as it comes and goes.
async fn follow_conn(shared: Arc<Shared>, mut conn: tokio::sync::watch::Receiver<Conn>) {
    loop {
        let c = conn.borrow_and_update().clone();
        match c {
            Conn::Connecting => shared.event(Event::Status(ConnState::Connecting, None)),
            Conn::Connected(session) => {
                shared.event(Event::Status(ConnState::Connected, None));
                shared.event(Event::Connected {
                    session: session.id().to_string(),
                    relay: session.relay().clone(),
                });
            }
            Conn::Down {
                reason, incompatible, ..
            } => {
                for feed in shared.feeds() {
                    feed.reset();
                }
                let reason = if incompatible {
                    format!("{reason} (update the app or the relay)")
                } else {
                    reason
                };
                shared.event(Event::Status(ConnState::Closed, Some(reason)));
            }
        }
        if conn.changed().await.is_err() {
            return;
        }
    }
}

/// Passes what `f` picks from a session broadcast on to the UI.
async fn forward<T: Clone>(shared: Arc<Shared>, mut rx: broadcast::Receiver<T>, f: impl Fn(T) -> Option<Event>) {
    loop {
        match rx.recv().await {
            Ok(m) => {
                if let Some(e) = f(m) {
                    shared.event(e);
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

/// What a subscription feeds.
enum Sub {
    Video(Arc<Feed>),
    Status,
    Events,
}

/// Subscription tasks by track; aborted (so unsubscribed) when dropped.
#[derive(Default)]
struct Subs(HashMap<Path, tokio::task::JoinHandle<()>>);

impl Drop for Subs {
    fn drop(&mut self) {
        for h in self.0.values() {
            h.abort();
        }
    }
}

/// Follows the room's state: passes it to the UI and keeps one
/// subscription per video, `.status` and `.events` track. Subscriptions
/// are the client's, so they carry on across reconnects.
async fn follow_room(shared: Arc<Shared>, client: Client) {
    let mut room = client.room();
    let mut subs = Subs::default();
    loop {
        let state = room.borrow_and_update().clone();
        let mut wanted = HashSet::new();
        for b in &state.broadcasts {
            for t in &b.tracks {
                let path = Path::new(&b.name, &t.info.id);
                let sub = match (t.info.kind, t.info.id.as_str()) {
                    (TrackKind::Video, _) => Sub::Video(shared.feed_for(&path, track_label(&b.name, &t.info))),
                    (TrackKind::Data, STATUS) => Sub::Status,
                    (TrackKind::Data, EVENTS) => Sub::Events,
                    _ => continue,
                };
                wanted.insert(path.clone());
                if subs.0.contains_key(&path) {
                    continue;
                }
                let reader = client.subscribe(path.clone());
                let task = read_track(shared.clone(), reader, sub);
                subs.0.insert(path, tokio::spawn(task));
            }
        }
        subs.0.retain(|p, h| {
            let keep = wanted.contains(p);
            if !keep {
                h.abort();
            }
            keep
        });
        shared.event(Event::Room(state));
        if room.changed().await.is_err() {
            return;
        }
    }
}

async fn read_track(shared: Arc<Shared>, mut reader: tq::Reader, sub: Sub) {
    let path = reader.path().clone();
    while let Some(f) = reader.recv().await {
        match &sub {
            Sub::Video(feed) => {
                // Arrival time, not now: time spent queued in the SDK is
                // decoder backlog too.
                let queued = Duration::from_micros(tq_wire::now_us().saturating_sub(f.recv_us));
                let now = Instant::now();
                feed.push(Job {
                    group: f.group,
                    codec: f.codec,
                    header: f.header,
                    payload: f.payload,
                    recv_us: f.recv_us,
                    recv_at: now.checked_sub(queued).unwrap_or(now),
                    clocks: f.clocks,
                });
            }
            Sub::Status => match f.json::<serde_json::Value>() {
                Ok(v) => shared.event(Event::BroadcastStatus(path.broadcast.clone(), v)),
                Err(e) => tracing::debug!(track = %path, "bad status: {e}"),
            },
            Sub::Events => match f.json::<serde_json::Value>() {
                Ok(v) => {
                    // The relay replays the newest group on subscribing, so
                    // an event can be old: show when it happened.
                    let at = match f.capture_local_us() {
                        Some(at) => at,
                        None => match shared.session() {
                            Some(s) => s
                                .wait_owner_offset(&path.broadcast, CLOCK_WAIT)
                                .await
                                .map_or(f.recv_us, |o| o.to_local(f.header.capture_us)),
                            None => f.recv_us,
                        },
                    };
                    let line = format!("{}: {}", path.broadcast, describe_robot_event(&v));
                    shared.event(Event::LogAt(at, line));
                }
                Err(e) => tracing::debug!(track = %path, "bad event: {e}"),
            },
        }
    }
}

/// A line from a broadcast's `.events` track: its `type` and the rest.
fn describe_robot_event(v: &serde_json::Value) -> String {
    let Some(obj) = v.as_object() else {
        return v.to_string();
    };
    let kind = obj.get("type").and_then(|t| t.as_str()).unwrap_or("event");
    let rest: Vec<String> = obj
        .iter()
        .filter(|(k, _)| *k != "type")
        .map(|(k, v)| match v.as_str() {
            Some(s) => format!("{k}={s}"),
            None => format!("{k}={v}"),
        })
        .collect();
    if rest.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} {}", rest.join(" "))
    }
}

/// Acks and heartbeat echoes, for the teleop panel.
async fn read_replies(shared: Arc<Shared>, mut rx: broadcast::Receiver<Reply>) {
    loop {
        let r = match rx.recv().await {
            Ok(r) => r,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let mut t = shared.teleop.lock().unwrap();
        match r {
            Reply::Ack {
                rtt_us, status, detail, ..
            } => {
                t.ack_rtt.add(rtt_us as f64 / 1000.0);
                *t.acks.entry(format!("{status:?}").to_lowercase()).or_default() += 1;
                if !detail.is_empty() {
                    t.last_ack_detail = Some(detail);
                }
            }
            Reply::Echo { rtt_us, tripped, .. } => {
                t.heartbeat_rtt.add(rtt_us as f64 / 1000.0);
                t.tripped = Some(tripped);
            }
        }
    }
}

/// Sends a remote stop until the robot acks it, on whichever session is up.
async fn send_remote_stop(shared: Arc<Shared>, channel: Path) {
    let mut replies = shared.client.replies();
    let body = serde_json::json!({ "reason": "console" });
    let mut sent = HashSet::new();
    let give_up = Instant::now() + STOP_GIVE_UP;
    let mut tick = tokio::time::interval(STOP_RETRY);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if Instant::now() >= give_up {
                    shared.event(Event::Log(format!("STOP {channel}: NO ANSWER from the robot in 2 s")));
                    return;
                }
                if let Some(s) = shared.session() {
                    if let Ok(seq) = s.channel(channel.clone()).send_json(&body) {
                        sent.insert((s.id().to_string(), seq));
                    }
                }
            }
            r = replies.recv() => match r {
                Ok(Reply::Ack { channel: c, seq, status, detail, .. }) if c == channel => {
                    let Some(s) = shared.session() else { continue };
                    if !sent.contains(&(s.id().to_string(), seq)) {
                        continue;
                    }
                    let line = match status {
                        tq_wire::AckStatus::Ok => format!("STOP {channel}: robot stopped"),
                        other => format!("STOP {channel}: robot answered {other:?} {detail}"),
                    };
                    shared.event(Event::Log(line));
                    return;
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
        }
    }
}

/// Drive commands on the drive channel, 20 Hz while active. Stopping lets
/// the robot's watchdog trip, by design. (Heartbeats are the [`Hold`]'s.)
async fn send_teleop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(TELEOP);
    loop {
        tick.tick().await;
        let drive = *shared.drive.lock().unwrap();
        if !drive.active {
            continue;
        }
        let Some(session) = shared.session() else {
            continue;
        };
        let Some(lock) = drive_broadcast(&session.room().borrow()).map(|b| Path::new(&b.name, DRIVE)) else {
            continue;
        };
        let msg = serde_json::json!({
            "command": "drive",
            "args": { "throttle": drive.throttle, "steer": drive.steer },
        });
        let _ = session.channel(lock).send_json(&msg);
    }
}
