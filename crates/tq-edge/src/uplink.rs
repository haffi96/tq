//! The device's side of the relay: sessions (connect, publish, and again
//! after every drop), channel messages to their sinks, and `.status`. The
//! publication, its sources, sinks and teleop live across sessions.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use tq::{Client, Conn, Inbox, Publication, PublishOptions, ServerTrust};
use tq_teleop::Teleop;
use tq_wire::{now_us, sign_token, BuildInfo, Claims, Perms};

use crate::{
    config::{Config, Trust},
    sink::Sink,
    source,
};

/// Connects to the relay and publishes, again after every drop, until the
/// process ends.
pub async fn run(cfg: Arc<Config>, build: BuildInfo, publication: Arc<Publication>) {
    let opts = PublishOptions {
        max_lag: Duration::from_millis(cfg.relay.max_lag_ms),
    };
    let client = Client::new({
        let (cfg, build) = (cfg.clone(), build.clone());
        move || {
            let (cfg, build) = (cfg.clone(), build.clone());
            async move { options(&cfg, build).await }
        }
    });
    client.publish(&publication, opts);
    let mut conn = client.conn();
    let mut was_connecting = false;
    loop {
        let c = conn.borrow_and_update().clone();
        let connecting = matches!(c, Conn::Connecting);
        match &c {
            Conn::Connecting if was_connecting => {}
            Conn::Connecting => tracing::info!(relay = %cfg.relay.url, "connecting"),
            Conn::Connected(s) => tracing::info!(relay = %s.relay().long(), "connected to relay"),
            Conn::Down { reason, retry_in, .. } => tracing::warn!(%reason, ?retry_in, "relay session down"),
        }
        was_connecting = connecting;
        if conn.changed().await.is_err() {
            return;
        }
    }
}

/// For each attempt: a fresh token, and the trust fetched again.
async fn options(cfg: &Config, build: BuildInfo) -> anyhow::Result<tq::Options> {
    let r = &cfg.relay;
    let token = if r.token.is_empty() {
        sign_token(
            r.shared_secret.as_bytes(),
            &Claims {
                sub: cfg.broadcast.clone(),
                room: cfg.room.clone(),
                exp: now_us() / 1_000_000 + 24 * 3600,
                perms: Perms::robot(&cfg.broadcast),
            },
        )
    } else {
        r.token.clone()
    };
    let trust = match &r.trust {
        Trust::SystemRoots => ServerTrust::SystemRoots,
        Trust::Insecure => ServerTrust::Insecure,
        Trust::Hash { sha256 } => ServerTrust::Hashes(vec![tq_transport::cert::parse_hash(sha256)?]),
        Trust::HashUrl { url } => ServerTrust::Hashes(vec![tq::fetch_cert_hash(url).await?]),
    };
    Ok(tq::Options {
        relay_url: r.url.parse()?,
        room: cfg.room.clone(),
        token,
        trust,
        quic: r.quic.clone(),
        build,
    })
}

/// Channel messages, for as long as the device runs: teleop's to teleop,
/// the rest to their channel's sink.
pub async fn route(mut inbox: Inbox, teleop: Option<Arc<Teleop>>, sinks: HashMap<String, Sink>) {
    while let Some(msg) = inbox.recv().await {
        let msg = match &teleop {
            Some(t) => match t.handle(msg) {
                Some(m) => m,
                None => continue,
            },
            None => msg,
        };
        match sinks.get(&msg.channel) {
            Some(sink) => sink.deliver(msg),
            None => msg.reject("no sink for this channel"),
        }
    }
}

/// The device's `.status` track: teleop (if any) and each source's
/// counters. At once when teleop trips, re-arms or changes holder, and
/// otherwise every `interval_ms`.
pub async fn status(
    p: Arc<Publication>,
    teleop: Option<Arc<Teleop>>,
    sources: Vec<(String, Arc<source::Stats>)>,
    interval_ms: u64,
) {
    let Some(out) = p.data(".status") else { return };
    p.set_online(".status", true, "");
    p.set_online(".events", true, "");
    let mut teleop_status = teleop.as_ref().map(|t| t.status());
    let mut tick = tokio::time::interval(Duration::from_millis(interval_ms));
    loop {
        let t = teleop_status.as_mut().map(|s| s.borrow_and_update().clone());
        let tracks: Vec<_> = sources
            .iter()
            .map(|(id, s)| {
                serde_json::json!({
                    "id": id,
                    "frames": s.frames.load(Relaxed),
                    "codec": *s.codec.lock().unwrap(),
                })
            })
            .collect();
        let mut status = serde_json::json!({ "tracks": tracks });
        if let Some(t) = &t {
            status["teleop"] = serde_json::to_value(t).expect("status serializes");
        }
        out.send_json(&status);
        // Counters ride along with the next tick; only edges (trips,
        // holder changes) go out at once.
        let edge = |n: &tq_teleop::Status| (n.tripped, n.reason.clone(), n.holder.clone());
        match (&mut teleop_status, &t) {
            (Some(rx), Some(t)) => {
                let was = edge(t);
                tokio::select! {
                    _ = tick.tick() => {}
                    r = rx.wait_for(|n| edge(n) != was) => if r.is_err() { return },
                }
            }
            _ => {
                tick.tick().await;
            }
        }
    }
}

pub async fn keyframe_requests(p: Arc<Publication>) {
    let mut rx = p.keyframe_requests();
    while let Ok(track) = rx.recv().await {
        // Encoders are separate processes with no control hook; their
        // periodic IDR is what recovers the stream.
        tracing::debug!(%track, "keyframe requested (no encoder hook, waiting for periodic IDR)");
    }
}
