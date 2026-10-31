//! The robot's sessions with the relay: connect, publish, and again after
//! every drop. The publication, its sources and teleop live across them.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::{Duration, Instant},
};

use tq::{Inbox, Publication, PublishOptions, ServerTrust, Session};
use tq_teleop::Teleop;
use tq_wire::{now_us, sign_token, BuildInfo, Claims, Perms};

use crate::{
    config::{Config, Trust},
    source::Camera,
};

pub struct Uplink {
    pub build: BuildInfo,
    pub cfg: Config,
    pub publication: Arc<Publication>,
}

impl Uplink {
    /// Connects to the relay and keeps reconnecting with backoff.
    pub async fn run(&self) {
        let mut backoff = Duration::from_millis(250);
        loop {
            let started = Instant::now();
            match self.session().await {
                Ok(why) => tracing::warn!(%why, "relay session ended"),
                Err(e) => tracing::warn!(error = format!("{e:#}"), "relay session failed"),
            }
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
            sub: self.cfg.broadcast.clone(),
            room: self.cfg.room_id.clone(),
            exp: now_us() / 1_000_000 + 24 * 3600,
            perms: Perms::robot(&self.cfg.broadcast),
        };
        sign_token(self.cfg.shared_secret.as_bytes(), &claims)
    }

    async fn trust(&self) -> anyhow::Result<ServerTrust> {
        Ok(match &self.cfg.relay_trust {
            Trust::SystemRoots => ServerTrust::SystemRoots,
            Trust::Insecure => ServerTrust::Insecure,
            Trust::Hash { sha256 } => ServerTrust::Hashes(vec![tq_transport::cert::parse_hash(sha256)?]),
            Trust::HashUrl { url } => ServerTrust::Hashes(vec![tq::fetch_cert_hash(url).await?]),
        })
    }

    /// One session: returns why it ended.
    async fn session(&self) -> anyhow::Result<String> {
        tracing::info!(relay = %self.cfg.relay_url, "connecting");
        let session = Session::connect(tq::Options {
            relay_url: self.cfg.relay_url.parse()?,
            room: self.cfg.room_id.clone(),
            token: self.token(),
            trust: self.trust().await?,
            quic: self.cfg.quic.clone(),
            build: self.build.clone(),
        })
        .await?;
        tracing::info!(relay = %session.relay().long(), "connected to relay");
        let opts = PublishOptions {
            max_lag: Duration::from_millis(self.cfg.max_lag_ms),
        };
        if let Err(e) = session.publish_with(&self.publication, opts).await {
            session.close();
            return Err(e);
        }
        Ok(session.closed().await)
    }
}

/// Drive and e-stop messages, for as long as the robot runs.
pub async fn teleop(teleop: Arc<Teleop>, mut inbox: Inbox) {
    while let Some(msg) = inbox.recv().await {
        if let Some(other) = teleop.handle(msg) {
            other.reject("no handler for this channel");
        }
    }
}

/// The robot's `.status` track: teleop and cameras, at once when teleop
/// trips, re-arms or changes holder, and otherwise every `interval_ms`.
pub async fn status(p: Arc<Publication>, teleop: Arc<Teleop>, cameras: Vec<Arc<Camera>>, interval_ms: u64) {
    let Some(out) = p.data(".status") else { return };
    p.set_online(".status", true, "");
    p.set_online(".events", true, "");
    let mut teleop_status = teleop.status();
    let mut tick = tokio::time::interval(Duration::from_millis(interval_ms));
    loop {
        let t = teleop_status.borrow_and_update().clone();
        let cams: Vec<_> = cameras
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.config.id,
                    "frames": c.frames.load(Relaxed),
                    "codec": *c.detected_codec.lock().unwrap(),
                })
            })
            .collect();
        out.send_json(&serde_json::json!({ "teleop": t, "cameras": cams }));
        // Counters ride along with the next tick; only edges (trips,
        // holder changes) go out at once.
        let edge = |n: &tq_teleop::Status| (n.tripped, n.reason.clone(), n.holder.clone());
        let was = edge(&t);
        tokio::select! {
            _ = tick.tick() => {}
            r = teleop_status.wait_for(|n| edge(n) != was) => if r.is_err() { return },
        }
    }
}

pub async fn keyframe_requests(p: Arc<Publication>) {
    let mut rx = p.keyframe_requests();
    while let Ok(track) = rx.recv().await {
        // The encoder is a separate process with no control hook; its
        // periodic IDR is what recovers the stream.
        tracing::debug!(%track, "keyframe requested (no encoder hook, waiting for periodic IDR)");
    }
}
