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
use tq::{Reply, ServerTrust, Session};
use tq_wire::{
    now_us, BroadcastState, BuildInfo, Compat, Control, Offset, Path, Perms, RoomEvent, RoomState, TrackInfo, TrackKind,
};

use crate::{
    feed::{Feed, Job},
    stats::Win,
};

const HEARTBEAT: Duration = Duration::from_millis(100);
const TELEOP: Duration = Duration::from_millis(50);
const RECONNECT: Duration = Duration::from_secs(1);
/// After a relay with another protocol major: it will not change soon.
const RECONNECT_REFUSED: Duration = Duration::from_secs(10);
/// A failed subscription is tried again after this long.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The lock (and the channel behind it) an operator drives with.
pub const DRIVE: &str = "drive";
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
    /// The connected session, if any.
    session: Mutex<Option<Session>>,
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
        self.session.lock().unwrap().clone()
    }

    pub fn request_keyframe(&self, track: &Path, reason: &str) {
        if let Some(s) = self.session() {
            s.request_keyframe(track, reason);
        }
    }

    /// Relay clock and `broadcast`'s owner clock, each minus this machine's.
    pub fn offsets(&self, broadcast: &str) -> (Option<Offset>, Option<Offset>) {
        match self.session() {
            Some(s) => (s.relay_offset(), s.owner_offset(broadcast)),
            None => (None, None),
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

    /// Claims the drive lock (taking it from its holder with `force`).
    pub fn claim(self: &Arc<Self>, force: bool) {
        let Some((session, lock)) = self.drive_target() else {
            return;
        };
        let shared = self.clone();
        self.rt.spawn(async move {
            let line = match session.claim(lock.clone(), force).await {
                Ok(Ok(())) => {
                    // Right away: the room's state names the holder a
                    // little later, and the watchdog is already counting.
                    let _ = session.heartbeat(&lock);
                    format!("Claim granted: you are driving {}", lock.broadcast)
                }
                Ok(Err((_, reason))) => format!("Claim refused: {reason}"),
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
        let shared = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = session.release(lock).await {
                shared.event(Event::Log(format!("Release failed: {e:#}")));
            }
        });
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
    let shared = Arc::new(Shared {
        config,
        teleop: Mutex::default(),
        drive: Mutex::default(),
        feeds: Mutex::default(),
        session: Mutex::default(),
        rt: rt.handle().clone(),
        events,
        frame_pending: Default::default(),
    });
    rt.spawn(run(shared.clone()));
    shared
}

async fn run(shared: Arc<Shared>) {
    loop {
        shared.event(Event::Status(ConnState::Connecting, None));
        let mut wait = RECONNECT;
        let reason = match connect(&shared).await {
            Ok(session) => {
                *shared.session.lock().unwrap() = Some(session.clone());
                shared.event(Event::Status(ConnState::Connected, None));
                shared.event(Event::Connected {
                    session: session.id().to_string(),
                    relay: session.relay().clone(),
                });
                let reason = serve(&shared, &session).await;
                *shared.session.lock().unwrap() = None;
                session.close();
                reason
            }
            Err(e) => {
                if e.is::<VersionRefused>() {
                    wait = RECONNECT_REFUSED;
                }
                format!("{e:#}")
            }
        };
        for feed in shared.feeds() {
            feed.reset();
        }
        shared.event(Event::Status(ConnState::Closed, Some(reason)));
        tokio::time::sleep(wait).await;
    }
}

#[derive(serde::Deserialize)]
struct Info {
    quic_port: u16,
    sha256: String,
    self_signed: bool,
    #[serde(default)]
    build: Option<BuildInfo>,
}

/// The relay speaks another protocol major; the app does not connect.
#[derive(Debug)]
pub struct VersionRefused(pub String);

impl std::fmt::Display for VersionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "incompatible relay: {} (update the app or the relay)", self.0)
    }
}

impl std::error::Error for VersionRefused {}

async fn connect(shared: &Shared) -> anyhow::Result<Session> {
    let c = &shared.config;
    let info: Info = http_get_json(&c.http.join("/api/info")?)
        .await
        .context("relay /api/info")?;
    if Compat::of_build(info.build.as_ref()) == Compat::Incompatible {
        return Err(VersionRefused(tq_wire::mismatch("relay", info.build.as_ref(), "tq-app")).into());
    }
    let token = match (&c.token, &c.secret) {
        (Some(t), _) => t.clone(),
        (None, Some(secret)) => tq_wire::sign_token(
            secret.as_bytes(),
            &tq_wire::Claims {
                sub: format!("app-{}", c.preset),
                room: c.room.clone(),
                exp: now_us() / 1_000_000 + 12 * 3600,
                perms: Perms::preset(&c.preset).with_context(|| format!("unknown preset '{}'", c.preset))?,
            },
        ),
        (None, None) => {
            let mut url = c.http.join("/api/dev/token")?;
            url.query_pairs_mut()
                .append_pair("preset", &c.preset)
                .append_pair("room", &c.room)
                .append_pair("sub", &format!("app-{}", c.preset));
            let v: serde_json::Value = http_get_json(&url)
                .await
                .context("no --token and the relay does not mint dev tokens")?;
            v["token"].as_str().context("no token in response")?.to_string()
        }
    };
    let trust = if info.self_signed {
        ServerTrust::Hashes(vec![tq_transport::cert::parse_hash(&info.sha256)?])
    } else {
        ServerTrust::SystemRoots
    };
    let host = match &c.relay {
        Some(r) => r.clone(),
        None => {
            let h = c.http.host_str().context("relay URL has no host")?;
            let h = if h.contains(':') && !h.starts_with('[') {
                format!("[{h}]")
            } else {
                h.to_string()
            };
            format!("{h}:{}", info.quic_port)
        }
    };
    Session::connect(tq::Options {
        relay_url: url::Url::parse(&format!("https://{host}"))?,
        room: c.room.clone(),
        token,
        trust,
        quic: Default::default(),
        build: c.build.clone(),
    })
    .await
}

/// Runs one connected session until it closes; returns why.
async fn serve(shared: &Arc<Shared>, session: &Session) -> String {
    // Taken before anything runs, so nothing that follows the welcome is
    // missed.
    let (events, control, replies) = (session.events(), session.control(), session.replies());
    // Dropped (and so aborted) when the session ends.
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(follow_room(shared.clone(), session.clone()));
    tasks.spawn(forward(shared.clone(), events, |e| Some(Event::RoomEvent(e))));
    tasks.spawn(forward(shared.clone(), control, |m| match m {
        Control::LinkStats { .. } | Control::Error { .. } => Some(Event::Control(m)),
        _ => None,
    }));
    tasks.spawn(read_replies(shared.clone(), replies));
    if shared.config.operator() {
        tasks.spawn(send_teleop(shared.clone(), session.clone()));
    }
    session.closed().await
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
/// subscription per video, `.status` and `.events` track.
async fn follow_room(shared: Arc<Shared>, session: Session) {
    let mut room = session.room();
    let mut subs = Subs::default();
    let mut changed = true;
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
                if subs.0.get(&path).is_some_and(|h| !h.is_finished()) {
                    continue;
                }
                let task = read_track(shared.clone(), session.clone(), path.clone(), sub);
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
        if changed {
            shared.event(Event::Room(state));
        }
        // Also wake now and then to retry failed subscriptions.
        tokio::select! {
            r = room.changed() => {
                if r.is_err() {
                    return;
                }
                changed = true;
            }
            _ = tokio::time::sleep(RESUBSCRIBE) => changed = false,
        }
    }
}

async fn read_track(shared: Arc<Shared>, session: Session, path: Path, sub: Sub) {
    let mut reader = match session.subscribe(path.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(track = %path, error = format!("{e:#}"), "subscribe failed");
            tokio::time::sleep(RESUBSCRIBE).await;
            return;
        }
    };
    tracing::debug!(track = %path, "subscribed");
    while let Some(f) = reader.recv().await {
        match &sub {
            Sub::Video(feed) => {
                // Arrival time, not now: time spent queued in the SDK is
                // decoder backlog too.
                let queued = Duration::from_micros(now_us().saturating_sub(f.recv_us));
                let now = Instant::now();
                feed.push(Job {
                    group: f.group,
                    codec: f.codec,
                    header: f.header,
                    payload: f.payload,
                    recv_us: f.recv_us,
                    recv_at: now.checked_sub(queued).unwrap_or(now),
                });
            }
            Sub::Status => match f.json::<serde_json::Value>() {
                Ok(v) => shared.event(Event::BroadcastStatus(path.broadcast.clone(), v)),
                Err(e) => tracing::debug!(track = %path, "bad status: {e}"),
            },
            Sub::Events => match f.json::<serde_json::Value>() {
                Ok(v) => shared.event(Event::Log(format!("{}: {}", path.broadcast, describe_robot_event(&v)))),
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

/// Heartbeats on the drive lock while this session holds it (100 ms), and
/// drive commands on its channel (20 Hz while active). Stopping either lets
/// the robot's watchdog trip, by design.
async fn send_teleop(shared: Arc<Shared>, session: Session) {
    let room = session.room();
    let mut tick = tokio::time::interval(TELEOP);
    let mut last_hb: Option<Instant> = None;
    loop {
        tick.tick().await;
        let (lock, mine) = {
            let r = room.borrow();
            let Some(b) = drive_broadcast(&r) else {
                continue;
            };
            let holder = b.lock(DRIVE).and_then(|l| l.holder.as_deref());
            (Path::new(&b.name, DRIVE), holder == Some(session.id()))
        };
        if mine && last_hb.is_none_or(|t| t.elapsed() >= HEARTBEAT) {
            last_hb = Some(Instant::now());
            let _ = session.heartbeat(&lock);
        }
        let drive = *shared.drive.lock().unwrap();
        if drive.active {
            let msg = serde_json::json!({
                "command": "drive",
                "args": { "throttle": drive.throttle, "steer": drive.steer },
            });
            let _ = session.channel(lock).send_json(&msg);
        }
    }
}

/// Minimal HTTP/1.0 GET for the relay's JSON endpoints: plain for `http://`
/// (dev, self-signed relays), TLS over the system roots for `https://`.
async fn http_get_json<T: serde::de::DeserializeOwned>(url: &url::Url) -> anyhow::Result<T> {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, request: &str) -> anyhow::Result<Vec<u8>> {
        s.write_all(request.as_bytes()).await?;
        // A TLS stream may still hold the encrypted request until flushed.
        s.flush().await?;
        let mut body = Vec::new();
        match s.read_to_end(&mut body).await {
            Ok(_) => {}
            // Some servers close without a TLS close_notify; the HTTP/1.0
            // body ends at the close either way.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !body.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(body)
    }

    let host = url.host_str().context("no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = format!(
        "{}:{port}",
        if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        }
    );
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    // 5 s for the whole request, of which at most 3 s to connect.
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let tcp = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(&addr))
            .await
            .context("connect timeout")??;
        match url.scheme() {
            "https" => {
                let tls = tokio_rustls::TlsConnector::from(Arc::new(tq_transport::quic::https_client_config()?));
                let name = rustls_pki_name(host)?;
                exchange(tls.connect(name, tcp).await?, &request).await
            }
            "http" => exchange(tcp, &request).await,
            other => anyhow::bail!("unsupported scheme {other}://"),
        }
    })
    .await
    .context("timeout")??;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").context("bad HTTP response")?;
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status != "200" {
        anyhow::bail!("HTTP {status}: {}", body.trim());
    }
    Ok(serde_json::from_str(body)?)
}

fn rustls_pki_name(host: &str) -> anyhow::Result<tokio_rustls::rustls::pki_types::ServerName<'static>> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string()).context("invalid TLS server name")
}
