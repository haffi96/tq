//! The relay session: the native counterpart of `web/src/worker.ts`.
//!
//! Runs on a tokio runtime next to the UI. It connects (and reconnects) to
//! the relay, reads the control stream, every camera's GOP streams and the
//! teleop datagrams, and hands frames to one decoder thread per camera
//! ([`crate::feed`]). The UI hears about connection and control changes
//! through [`Event`]s and drives the session with [`Command`]s.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use futures::channel::mpsc::UnboundedSender;
use tokio::sync::mpsc;
use tq_proto::{clock_offset, now_us, BuildInfo, CameraInfo, ClockFilter, Compat, Control, Datagram, Role};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::GroupReader,
    quic::{self, QuicOptions, ServerTrust},
    Session,
};

use crate::{
    feed::{Feed, Job},
    stats::Win,
};

const PING: Duration = Duration::from_millis(500);
const HEARTBEAT: Duration = Duration::from_millis(100);
const TELEOP: Duration = Duration::from_millis(50);
const RECONNECT: Duration = Duration::from_secs(1);
/// After a relay with another protocol major: it will not change soon.
const RECONNECT_REFUSED: Duration = Duration::from_secs(10);
/// Group streams can overtake the welcome message; they wait this long for
/// the camera list rather than being dropped (which would lose the cached
/// GOP).
const CAMERAS_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Config {
    /// Relay HTTP base, for `/api/info` and dev tokens.
    pub http: url::Url,
    /// `host:port` of the relay's QUIC endpoint; default: the HTTP host and
    /// the port from `/api/info`.
    pub relay: Option<String>,
    pub av: String,
    pub role: Role,
    pub token: Option<String>,
    /// Signs a token locally instead of asking the relay for a dev token.
    pub secret: Option<String>,
    pub log_stats: bool,
    /// This app's build, sent in `hello`.
    pub build: BuildInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Closed,
}

/// Session -> UI. Low rate, so the large `Control` variant is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    Status(ConnState, Option<String>),
    Control(Control),
    /// A camera got a new picture (coalesced: at most one pending).
    Frame,
}

/// UI -> session.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
    Control(Control),
    /// One teleop command datagram.
    Teleop {
        command: String,
        args: serde_json::Value,
    },
}

/// Clock offsets, microseconds. Mine is this app minus the relay, the
/// publisher's is the publisher minus the relay; each with its error bound.
#[derive(Debug, Clone, Default)]
pub struct Clock {
    pub my_offset_us: Option<i64>,
    pub my_error_us: Option<i64>,
    pub pub_offset_us: Option<i64>,
    pub pub_error_us: Option<i64>,
    pub relay_rtt_ms: Option<f64>,
}

impl Clock {
    /// Bound on the error of the publisher -> app clock mapping (both hops).
    pub fn sync_error_ms(&self) -> Option<f64> {
        Some((self.my_error_us? + self.pub_error_us?) as f64 / 1000.0)
    }
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
    /// Vehicle dead-man state as seen in heartbeat echoes.
    pub tripped: Option<bool>,
}

/// State shared by the session, the decoder threads and the UI.
pub struct Shared {
    pub config: Config,
    pub clock: Mutex<Clock>,
    pub teleop: Mutex<Teleop>,
    pub drive: Mutex<Drive>,
    /// Feeds by camera id, in announce order. Kept across reconnects so the
    /// UI tiles stay put.
    pub feeds: Mutex<Vec<Arc<Feed>>>,
    /// This session's camera list: index on group streams -> camera id.
    cameras: Mutex<Vec<String>>,
    cameras_known: tokio::sync::Notify,
    events: UnboundedSender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    frame_pending: std::sync::atomic::AtomicBool,
}

impl Shared {
    pub fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
    }

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

    pub fn request_keyframe(&self, camera: &str, reason: &str) {
        self.send(Command::Control(Control::KeyframeRequest {
            camera: camera.to_string(),
            reason: reason.to_string(),
        }));
    }

    fn event(&self, e: Event) {
        let _ = self.events.unbounded_send(e);
    }

    fn set_cameras(self: &Arc<Self>, list: &[CameraInfo]) {
        let mut feeds = self.feeds.lock().unwrap();
        for info in list {
            if !feeds.iter().any(|f| f.info.id == info.id) {
                feeds.push(Feed::spawn(info.clone(), Arc::downgrade(self)));
            }
        }
        *self.cameras.lock().unwrap() = list.iter().map(|c| c.id.clone()).collect();
        self.cameras_known.notify_waiters();
    }

    fn feed_at(&self, index: u16) -> Option<Arc<Feed>> {
        let id = self.cameras.lock().unwrap().get(index as usize).cloned()?;
        self.feeds.lock().unwrap().iter().find(|f| f.info.id == id).cloned()
    }
}

/// Starts the session on `rt`. Events for the UI arrive on `events`.
pub fn start(rt: &tokio::runtime::Runtime, config: Config, events: UnboundedSender<Event>) -> Arc<Shared> {
    let (commands, rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        config,
        clock: Mutex::default(),
        teleop: Mutex::default(),
        drive: Mutex::default(),
        feeds: Mutex::default(),
        cameras: Mutex::default(),
        cameras_known: tokio::sync::Notify::new(),
        events,
        commands,
        frame_pending: Default::default(),
    });
    rt.spawn(run(shared.clone(), rx));
    shared
}

async fn run(shared: Arc<Shared>, mut commands: mpsc::UnboundedReceiver<Command>) {
    loop {
        shared.event(Event::Status(ConnState::Connecting, None));
        let mut wait = RECONNECT;
        let reason = match connect(&shared).await {
            Ok((session, relay)) => {
                shared.event(Event::Status(ConnState::Connected, None));
                let reason = serve(&shared, session, relay.is_some(), &mut commands).await;
                // Commands queued while disconnected are stale (teleop
                // especially); drop them.
                while commands.try_recv().is_ok() {}
                reason
            }
            Err(e) => {
                if e.is::<VersionRefused>() {
                    wait = RECONNECT_REFUSED;
                }
                format!("{e:#}")
            }
        };
        *shared.cameras.lock().unwrap() = Vec::new();
        *shared.clock.lock().unwrap() = Clock::default();
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
    /// Relays from protocol 1.1 on.
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

/// Connects; returns the session and the relay's build (`None` before 1.1).
async fn connect(shared: &Shared) -> anyhow::Result<(Session, Option<BuildInfo>)> {
    let c = &shared.config;
    let info: Info = http_get_json(&c.http.join("/api/info")?)
        .await
        .context("relay /api/info")?;
    if Compat::of_build(info.build.as_ref()) == Compat::Incompatible {
        return Err(VersionRefused(tq_proto::mismatch("relay", info.build.as_ref(), "tq-app")).into());
    }
    let token = match (&c.token, &c.secret) {
        (Some(t), _) => t.clone(),
        (None, Some(secret)) => tq_proto::sign_token(
            secret.as_bytes(),
            &tq_proto::Claims {
                sub: format!("app-{}", c.role),
                role: c.role,
                av: c.av.clone(),
                exp: now_us() / 1_000_000 + 12 * 3600,
            },
        ),
        (None, None) => {
            let mut url = c.http.join("/api/dev/token")?;
            url.query_pairs_mut()
                .append_pair("role", &c.role.to_string())
                .append_pair("av", &c.av)
                .append_pair("sub", &format!("app-{}", c.role));
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
    let client = quic::client(trust, &QuicOptions::default())?;
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
    let mut url = url::Url::parse(&format!("https://{host}"))?;
    url.set_path(&format!("/v1/session/{}", c.av));
    url.query_pairs_mut().append_pair("token", &token);
    let session = client.connect(url).await.context("WebTransport connect")?;
    Ok((session, info.build))
}

/// Runs one connected session until it closes; returns why. `hello`: the
/// relay knows the message (it reported a build).
async fn serve(
    shared: &Arc<Shared>,
    session: Session,
    hello: bool,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> String {
    let (send, recv) = match session.open_bi().await {
        Ok(s) => s,
        Err(e) => return format!("control stream: {e}"),
    };
    let ctl = ControlTx::new(send);
    if hello {
        let build = shared.config.build.clone();
        if let Err(e) = ctl.send(&Control::Hello { build }).await {
            return format!("control: {e}");
        }
    }
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(read_control(shared.clone(), ControlRx::new(recv)));
    tasks.spawn(read_groups(shared.clone(), session.clone()));
    tasks.spawn(read_datagrams(shared.clone(), session.clone()));
    if shared.config.role == Role::Operator {
        tasks.spawn(send_teleop(shared.clone(), session.clone()));
    }

    let mut ping = tokio::time::interval(PING);
    let mut ping_id = 0u64;
    let mut teleop_seq = 0u32;
    loop {
        tokio::select! {
            _ = ping.tick() => {
                ping_id += 1;
                if let Err(e) = ctl.send(&Control::Ping { id: ping_id, t0_us: now_us() }).await {
                    return format!("control: {e}");
                }
            }
            Some(cmd) = commands.recv() => match cmd {
                Command::Control(msg) => {
                    tracing::debug!(?msg, "control ->");
                    if let Err(e) = ctl.send(&msg).await {
                        return format!("control: {e}");
                    }
                }
                Command::Teleop { command, args } => {
                    teleop_seq = teleop_seq.wrapping_add(1);
                    let _ = session.send_datagram(command_datagram(teleop_seq, &command, &args));
                }
            },
            e = session.closed() => return format!("closed: {e}"),
            Some(r) = tasks.join_next() => {
                if let Ok(Err(e)) = r {
                    return format!("{e:#}");
                }
            }
        }
    }
}

fn command_datagram(seq: u32, command: &str, args: &serde_json::Value) -> bytes::Bytes {
    let body = serde_json::to_vec(&serde_json::json!({ "command": command, "args": args })).unwrap();
    Datagram::Command {
        seq,
        sent_us: now_us(),
        body: body.into(),
    }
    .encode()
}

async fn read_control(shared: Arc<Shared>, mut rx: ControlRx) -> anyhow::Result<()> {
    let mut filter = ClockFilter::new(20);
    while let Some(msg) = rx.recv().await? {
        match &msg {
            Control::Welcome { state, .. } | Control::State { state } => {
                let mut clock = shared.clock.lock().unwrap();
                clock.pub_offset_us = state.publisher.as_ref().and_then(|p| p.clock_offset_us);
                clock.pub_error_us = state.publisher.as_ref().and_then(|p| p.clock_error_us);
                drop(clock);
                if let Some(p) = &state.publisher {
                    shared.set_cameras(&p.cameras);
                }
            }
            Control::Pong { t0_us, t1_us, .. } => {
                let (relay_minus_me, rtt) = clock_offset(*t0_us, *t1_us, now_us());
                filter.add(-relay_minus_me, rtt);
                let mut clock = shared.clock.lock().unwrap();
                if let Some((off, err)) = filter.estimate() {
                    clock.my_offset_us = Some(off);
                    clock.my_error_us = Some(err);
                }
                clock.relay_rtt_ms = Some(rtt as f64 / 1000.0);
                continue;
            }
            _ => {}
        }
        shared.event(Event::Control(msg));
    }
    anyhow::bail!("relay closed the control stream")
}

async fn read_groups(shared: Arc<Shared>, session: Session) -> anyhow::Result<()> {
    loop {
        let stream = session.accept_uni().await?;
        let shared = shared.clone();
        tokio::spawn(async move {
            let _ = read_group(shared, stream).await;
        });
    }
}

async fn read_group(shared: Arc<Shared>, stream: tq_transport::RecvStream) -> anyhow::Result<()> {
    let mut reader = GroupReader::open(stream).await?;
    let header = reader.header.clone();
    let mut feed = shared.feed_at(header.camera);
    if feed.is_none() {
        let known = shared.cameras_known.notified();
        let _ = tokio::time::timeout(CAMERAS_WAIT, known).await;
        feed = shared.feed_at(header.camera);
    }
    let Some(feed) = feed else {
        reader.stop();
        return Ok(());
    };
    // A newer group supersedes older ones: it starts with a keyframe, so
    // nothing in them is needed any more.
    if !feed.start_group(header.group) {
        reader.stop();
        return Ok(());
    }
    let codec: Arc<str> = header.codec.into();
    while let Some((frame, payload)) = reader.next().await? {
        if !feed.is_current(header.group) {
            reader.stop();
            return Ok(());
        }
        feed.push(Job {
            codec: codec.clone(),
            header: frame,
            payload,
            recv_us: now_us(),
            recv_at: Instant::now(),
        });
    }
    Ok(())
}

async fn read_datagrams(shared: Arc<Shared>, session: Session) -> anyhow::Result<()> {
    loop {
        let d = session.read_datagram().await?;
        let now = now_us();
        let mut t = shared.teleop.lock().unwrap();
        match Datagram::decode(d) {
            Ok(Datagram::Ack {
                echo_us,
                status,
                detail,
                ..
            }) => {
                t.ack_rtt.add(now.saturating_sub(echo_us) as f64 / 1000.0);
                *t.acks.entry(format!("{status:?}").to_lowercase()).or_default() += 1;
                if !detail.is_empty() {
                    t.last_ack_detail = Some(detail);
                }
            }
            Ok(Datagram::Echo { echo_us, tripped, .. }) => {
                t.heartbeat_rtt.add(now.saturating_sub(echo_us) as f64 / 1000.0);
                t.tripped = Some(tripped);
            }
            _ => {}
        }
    }
}

/// Operator heartbeats (100 ms) and drive commands (20 Hz while active).
/// Stopping the commands lets the vehicle trip its dead-man after
/// `command_stale_ms`, by design.
async fn send_teleop(shared: Arc<Shared>, session: Session) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(TELEOP);
    let mut last_hb = Instant::now() - HEARTBEAT;
    let (mut seq, mut hb) = (0x8000_0000u32, 0u32);
    loop {
        tick.tick().await;
        if last_hb.elapsed() >= HEARTBEAT {
            last_hb = Instant::now();
            hb = hb.wrapping_add(1);
            let _ = session.send_datagram(
                Datagram::Heartbeat {
                    seq: hb,
                    sent_us: now_us(),
                }
                .encode(),
            );
        }
        let drive = *shared.drive.lock().unwrap();
        if drive.active {
            seq = seq.wrapping_add(1);
            let args = serde_json::json!({ "throttle": drive.throttle, "steer": drive.steer });
            let _ = session.send_datagram(command_datagram(seq, "drive", &args));
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
