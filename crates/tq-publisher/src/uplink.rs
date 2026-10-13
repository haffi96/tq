//! The publisher's session with the relay.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::{Duration, Instant},
};

use anyhow::Context;
use tokio::{sync::broadcast::error::RecvError, task::JoinSet};
use tq_proto::{now_us, sign_token, BuildInfo, CameraInfo, CameraStatus, Claims, Compat, Control, Datagram, Role};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::{GroupSender, Outcome, SendPolicy},
    quic::{self, ServerTrust},
    Session,
};

use crate::{
    config::{Config, Trust},
    source::Camera,
    teleop::Teleop,
};

pub struct Uplink {
    pub build: BuildInfo,
    pub cfg: Config,
    pub cameras: Vec<Arc<Camera>>,
    pub teleop: Arc<Teleop>,
}

impl Uplink {
    /// Connects to the relay and keeps reconnecting with backoff.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_millis(250);
        loop {
            let started = Instant::now();
            match self.session().await {
                Ok(()) => tracing::warn!("relay session ended"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "relay session failed"),
            }
            self.teleop.connection_lost();
            if started.elapsed() > Duration::from_secs(10) {
                backoff = Duration::from_millis(250);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    fn token(&self) -> String {
        if !self.cfg.token.is_empty() {
            return self.cfg.token.clone();
        }
        let claims = Claims {
            sub: self.cfg.publisher_id.clone(),
            role: Role::Publisher,
            av: self.cfg.av_id.clone(),
            exp: now_us() / 1_000_000 + 24 * 3600,
        };
        sign_token(self.cfg.shared_secret.as_bytes(), &claims)
    }

    async fn trust(&self) -> anyhow::Result<ServerTrust> {
        Ok(match &self.cfg.relay_trust {
            Trust::SystemRoots => ServerTrust::SystemRoots,
            Trust::Insecure => ServerTrust::Insecure,
            Trust::Hash { sha256 } => ServerTrust::Hashes(vec![tq_transport::cert::parse_hash(sha256)?]),
            Trust::HashUrl { url } => ServerTrust::Hashes(vec![fetch_cert_hash(url).await?]),
        })
    }

    async fn session(&self) -> anyhow::Result<()> {
        let client = quic::client(self.trust().await?, &self.cfg.quic)?;
        let mut url = url::Url::parse(&self.cfg.relay_url)?;
        url.set_path(&format!("/v1/publish/{}", self.cfg.av_id));
        url.query_pairs_mut().append_pair("token", &self.token());
        tracing::info!(relay = %self.cfg.relay_url, "connecting");
        let session = tokio::time::timeout(Duration::from_secs(5), client.connect(url))
            .await
            .context("connect timed out")?
            .context("connect")?;
        let (send, recv) = session.open_bi().await?;
        let ctl = ControlTx::new(send);
        let mut rx = ControlRx::new(recv);

        let cameras: Vec<CameraInfo> = self
            .cameras
            .iter()
            .map(|c| CameraInfo {
                id: c.config.id.clone(),
                label: c.config.label.clone(),
                codec: c
                    .stats
                    .detected_codec
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| c.config.codec.clone()),
                width: c.config.width,
                height: c.config.height,
                fps: c.config.fps,
            })
            .collect();
        ctl.send(&Control::Announce {
            av_id: self.cfg.av_id.clone(),
            publisher_id: self.cfg.publisher_id.clone(),
            cameras,
            build: Some(self.build.clone()),
        })
        .await?;
        let relay = match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .context("announce timed out")??
        {
            Some(Control::AnnounceOk { relay, .. }) => relay,
            Some(Control::Error { message }) => anyhow::bail!("relay refused announce: {message}"),
            other => anyhow::bail!("unexpected reply to announce: {other:?}"),
        };
        match Compat::of_build(relay.as_ref()) {
            Compat::Same => {}
            Compat::MinorDiffers => tracing::warn!("{}", tq_proto::mismatch("relay", relay.as_ref(), "tq-publisher")),
            Compat::Incompatible => {
                let reason = tq_proto::mismatch("relay", relay.as_ref(), "tq-publisher");
                session.close(tq_proto::code::UNSUPPORTED_VERSION, reason.as_bytes());
                anyhow::bail!("incompatible relay: {reason}");
            }
        }
        let relay_version = relay
            .as_ref()
            .map_or_else(|| "unknown (protocol 1.0)".to_string(), |b| b.long());
        tracing::info!(relay = %relay_version, "connected to relay");

        let mut tasks = JoinSet::new();
        for cam in &self.cameras {
            tasks.spawn(send_camera(session.clone(), cam.clone(), self.cfg.max_lag_ms));
        }
        tasks.spawn(control_loop(rx, ctl.clone(), self.teleop.clone()));
        tasks.spawn(datagram_loop(session.clone(), self.teleop.clone()));
        tasks.spawn(status_loop(
            ctl,
            self.cameras.clone(),
            self.teleop.clone(),
            self.cfg.status_interval_ms,
        ));

        let result = tokio::select! {
            e = session.closed() => Err(anyhow::Error::from(e)),
            Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
        };
        tasks.abort_all();
        session.close(tq_proto::code::SHUTDOWN, b"bye");
        result
    }
}

/// Streams one camera's track to the relay with the uplink drop policy.
async fn send_camera(session: Session, cam: Arc<Camera>, max_lag_ms: u64) -> anyhow::Result<()> {
    let policy = SendPolicy {
        keep_groups: 0,
        max_lag: Duration::from_millis(max_lag_ms),
    };
    let mut sender = GroupSender::new(session, policy);
    let (cache, mut rx) = cam.track.subscribe();
    for f in cache {
        sender.send(&f, f.header.publisher_us).await?;
    }
    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(RecvError::Lagged(n)) => {
                cam.stats.dropped.fetch_add(n, Relaxed);
                sender.abandon();
                continue;
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        match sender.send(&frame, frame.header.publisher_us).await? {
            Outcome::Sent => {}
            Outcome::Skipped | Outcome::Late => {
                cam.stats.dropped.fetch_add(1, Relaxed);
            }
        }
    }
}

async fn control_loop(mut rx: ControlRx, ctl: ControlTx, teleop: Arc<Teleop>) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::Ping { id, t0_us } => {
                ctl.send(&Control::Pong {
                    id,
                    t0_us,
                    t1_us: now_us(),
                })
                .await?
            }
            Control::Operator { session } => teleop.set_operator(session),
            Control::KeyframeRequest { camera, reason } => {
                // The encoder is a separate process with no control hook;
                // its periodic IDR is what recovers the stream.
                tracing::debug!(%camera, %reason, "keyframe requested (no encoder hook, waiting for periodic IDR)");
            }
            Control::Error { message } => tracing::warn!(%message, "relay error"),
            other => tracing::debug!(?other, "ignored control message"),
        }
    }
    anyhow::bail!("relay closed the control stream")
}

async fn datagram_loop(session: Session, teleop: Arc<Teleop>) -> anyhow::Result<()> {
    loop {
        let data = session.read_datagram().await?;
        let dg = match Datagram::decode(data) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "bad datagram");
                continue;
            }
        };
        if let Some(reply) = teleop.handle(dg) {
            let _ = session.send_datagram(reply.encode());
        }
    }
}

async fn status_loop(
    ctl: ControlTx,
    cameras: Vec<Arc<Camera>>,
    teleop: Arc<Teleop>,
    interval_ms: u64,
) -> anyhow::Result<()> {
    let interval = Duration::from_millis(interval_ms);
    let mut last: Vec<(u64, u64)> = cameras
        .iter()
        .map(|c| (c.stats.frames.load(Relaxed), c.stats.bytes.load(Relaxed)))
        .collect();
    let mut last_at = Instant::now();
    let mut ticker = tokio::time::interval(interval);
    let mut teleop_state = teleop.status();
    let mut deadman_tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = deadman_tick.tick() => {
                // Push teleop changes (trip / re-arm) right away rather than
                // on the next status tick.
                let now = teleop.status();
                if now.deadman_tripped == teleop_state.deadman_tripped && now.operator == teleop_state.operator {
                    continue;
                }
            }
        }
        let secs = last_at.elapsed().as_secs_f64().max(1e-3);
        last_at = Instant::now();
        let mut status = Vec::with_capacity(cameras.len());
        for (c, prev) in cameras.iter().zip(last.iter_mut()) {
            let s = &c.stats;
            let (frames, bytes) = (s.frames.load(Relaxed), s.bytes.load(Relaxed));
            let lat_n = s.capture_lat_n.swap(0, Relaxed);
            let lat_sum = s.capture_lat_sum_us.swap(0, Relaxed);
            status.push(CameraStatus {
                id: c.config.id.clone(),
                online: s.online.load(Relaxed),
                codec_string: s.codec_string.lock().unwrap().clone(),
                frames,
                groups: s.groups.load(Relaxed),
                fps: (frames - prev.0) as f64 / secs,
                kbps: (bytes - prev.1) as f64 * 8.0 / 1000.0 / secs,
                dropped: s.dropped.load(Relaxed),
                capture_to_publisher_ms: (lat_n > 0).then(|| lat_sum as f64 / lat_n as f64 / 1000.0),
            });
            *prev = (frames, bytes);
        }
        teleop_state = teleop.status();
        ctl.send(&Control::PublisherStatus {
            cameras: status,
            teleop: teleop_state.clone(),
        })
        .await?;
    }
}

/// `GET` over plain HTTP/1.0, expecting `{"sha256": "<hex>"}`.
async fn fetch_cert_hash(url: &str) -> anyhow::Result<[u8; 32]> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let u = url::Url::parse(url)?;
    anyhow::ensure!(u.scheme() == "http", "relay_trust.url must be http:// (got {url})");
    let host = u.host_str().context("url has no host")?;
    let port = u.port().unwrap_or(80);
    let mut s = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect((host, port)))
        .await
        .context("cert hash fetch timed out")??;
    let req = format!("GET {} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n", u.path());
    s.write_all(req.as_bytes()).await?;
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut body)).await??;
    let text = String::from_utf8_lossy(&body);
    let json = text.split("\r\n\r\n").nth(1).context("bad HTTP response")?;
    let v: serde_json::Value = serde_json::from_str(json).context("cert hash response is not JSON")?;
    tq_transport::cert::parse_hash(v["sha256"].as_str().context("no sha256 field")?)
}
