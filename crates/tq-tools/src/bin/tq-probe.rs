//! tq-probe: a headless console. Joins a room, subscribes to every track
//! of every broadcast (or of `--broadcast`), and reports per-hop latency
//! for each; with `--teleop-hz` it claims a broadcast's drive lock (forced)
//! and streams drive commands and heartbeats to exercise the dead-man
//! path. It logs the room's events as they arrive.
//!
//! ```text
//! tq-probe --relay https://127.0.0.1:4443 --room robot-01 --preset operator --teleop-hz 20 --duration 20
//! ```

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use tq::{Reply, ServerTrust, Session};
use tq_wire::{now_us, sign_token, Claims, Path, Perms, TrackKind, FLAG_CAPTURE_FROM_SEI};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "https://127.0.0.1:4443")]
    relay: String,
    /// Fetch the relay certificate hash from here (plain HTTP).
    #[arg(long, default_value = "http://127.0.0.1:8080/api/cert")]
    cert_url: String,
    #[arg(long, default_value = "robot-01")]
    room: String,
    /// Token preset: `viewer` or `operator`.
    #[arg(long, alias = "role", default_value = "viewer")]
    preset: String,
    /// Only this broadcast (default: all of them).
    #[arg(long)]
    broadcast: Option<String>,
    #[arg(long, env = "TQ_SHARED_SECRET", hide_env_values = true)]
    secret: String,
    /// Seconds to run (0 = forever).
    #[arg(long, default_value_t = 10)]
    duration: u64,
    /// Claim (force) each broadcast's `drive` lock and send drive commands
    /// at this rate. Needs the operator preset.
    #[arg(long, default_value_t = 0.0)]
    teleop_hz: f64,
    /// Print one summary line as JSON at the end.
    #[arg(long)]
    json: bool,
    /// Claim this protocol (`MAJOR.MINOR`) in the hello, to test the
    /// relay's version checks.
    #[arg(long, hide = true)]
    fake_protocol: Option<String>,
}

#[derive(Default)]
struct Samples(Vec<f64>);

impl Samples {
    fn add(&mut self, v: f64) {
        self.0.push(v);
    }
    fn summary(&self) -> serde_json::Value {
        if self.0.is_empty() {
            return serde_json::Value::Null;
        }
        let mut v = self.0.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        let r = |x: f64| (x * 10.0).round() / 10.0;
        serde_json::json!({
            "n": v.len(),
            "mean": r(v.iter().sum::<f64>() / v.len() as f64),
            "p50": r(pct(0.5)),
            "p95": r(pct(0.95)),
            "max": r(pct(1.0)),
        })
    }
}

#[derive(Default)]
struct TrackStats {
    kind: String,
    frames: u64,
    groups: HashSet<u64>,
    bytes: u64,
    codec: String,
    capture_to_probe: Samples,
    publisher_to_relay: Samples,
    relay_to_probe: Samples,
}

#[derive(Default)]
struct Shared {
    tracks: HashMap<Path, TrackStats>,
    ack_rtt: Samples,
    heartbeat_rtt: Samples,
    acks: HashMap<String, u64>,
    tripped: Option<bool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();
    let args = Args::parse();
    let perms = Perms::preset(&args.preset).context("--preset: operator or viewer")?;
    let token = sign_token(
        args.secret.as_bytes(),
        &Claims {
            sub: format!("probe-{}", args.preset),
            room: args.room.clone(),
            exp: now_us() / 1_000_000 + 3600,
            perms,
        },
    );
    let mut build = tq_wire::build_info!("tq-probe");
    if let Some(p) = &args.fake_protocol {
        let (major, minor) = p.split_once('.').context("--fake-protocol MAJOR.MINOR")?;
        build.protocol = tq_wire::ProtocolVersion {
            major: major.parse()?,
            minor: minor.parse()?,
        };
    }
    let session = Session::connect(tq::Options {
        relay_url: args.relay.parse()?,
        room: args.room.clone(),
        token,
        trust: ServerTrust::Hashes(vec![tq::fetch_cert_hash(&args.cert_url).await?]),
        quic: Default::default(),
        build,
    })
    .await?;
    tracing::info!(session = %session.id(), relay = %session.relay().long(), perms = ?session.perms().0, "welcome");
    let shared = Arc::new(Mutex::new(Shared::default()));

    // The room's events.
    {
        let mut events = session.events();
        tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                tracing::info!(seq = e.seq, event = ?e.event, "event");
            }
        });
    }
    // Every track, as it appears.
    tokio::spawn(subscribe_all(session.clone(), args.broadcast.clone(), shared.clone()));
    if args.teleop_hz > 0.0 {
        tokio::spawn(teleop(session.clone(), args.broadcast.clone(), args.teleop_hz, shared.clone()));
    }

    let started = Instant::now();
    let mut report = tokio::time::interval(Duration::from_secs(2));
    report.tick().await;
    loop {
        tokio::select! {
            _ = report.tick() => {
                let s = shared.lock().unwrap();
                for (path, t) in &s.tracks {
                    tracing::info!(
                        "{path}: {} frames {} groups {} | capture->probe {} | pub->relay {} | relay->probe {}",
                        t.frames, t.groups.len(), t.codec,
                        t.capture_to_probe.summary(), t.publisher_to_relay.summary(), t.relay_to_probe.summary()
                    );
                }
                if args.teleop_hz > 0.0 {
                    tracing::info!("ack rtt {} heartbeat rtt {} acks {:?} tripped {:?}", s.ack_rtt.summary(), s.heartbeat_rtt.summary(), s.acks, s.tripped);
                }
            }
            why = session.closed() => anyhow::bail!("session closed: {why}"),
        }
        if args.duration > 0 && started.elapsed() >= Duration::from_secs(args.duration) {
            break;
        }
    }
    let s = shared.lock().unwrap();
    let secs = started.elapsed().as_secs_f64();
    let tracks: serde_json::Map<String, serde_json::Value> = s
        .tracks
        .iter()
        .map(|(path, t)| {
            (
                path.to_string(),
                serde_json::json!({
                    "kind": t.kind, "codec": t.codec, "frames": t.frames, "groups": t.groups.len(),
                    "fps": (t.frames as f64 / secs * 10.0).round() / 10.0,
                    "kbps": (t.bytes as f64 * 8.0 / 1000.0 / secs).round(),
                    "capture_to_probe_ms": t.capture_to_probe.summary(),
                    "publisher_to_relay_ms": t.publisher_to_relay.summary(),
                    "relay_to_probe_ms": t.relay_to_probe.summary(),
                }),
            )
        })
        .collect();
    let out = serde_json::json!({
        "tracks": tracks,
        "ack_rtt_ms": s.ack_rtt.summary(),
        "heartbeat_rtt_ms": s.heartbeat_rtt.summary(),
        "acks": s.acks,
        "deadman_tripped": s.tripped,
        "relay_offset_us": session.relay_offset().map(|o| o.offset_us),
        "quic": tq_transport::quic::quic_stats(session.transport()),
    });
    if args.json {
        println!("{out}");
    } else {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    session.close();
    Ok(())
}

/// Subscribes to each track once it is in the room.
async fn subscribe_all(session: Session, only: Option<String>, shared: Arc<Mutex<Shared>>) {
    let mut room = session.room();
    let mut seen = HashSet::new();
    loop {
        let paths: Vec<(Path, TrackKind)> = room
            .borrow_and_update()
            .broadcasts
            .iter()
            .filter(|b| only.as_ref().is_none_or(|o| *o == b.name))
            .flat_map(|b| b.tracks.iter().map(|t| (Path::new(&b.name, &t.info.id), t.info.kind)))
            .collect();
        for (path, kind) in paths {
            if seen.insert(path.clone()) {
                let (session, shared) = (session.clone(), shared.clone());
                tokio::spawn(async move {
                    if let Err(e) = read_track(session, path.clone(), kind, shared).await {
                        tracing::warn!(track = %path, error = format!("{e:#}"), "subscription failed");
                    }
                });
            }
        }
        if room.changed().await.is_err() {
            return;
        }
    }
}

async fn read_track(session: Session, path: Path, kind: TrackKind, shared: Arc<Mutex<Shared>>) -> anyhow::Result<()> {
    let mut reader = session.subscribe(path.clone()).await?;
    tracing::info!(track = %path, "subscribed");
    while let Some(f) = reader.recv().await {
        // Everything on this machine's clock.
        let relay = session.relay_offset().unwrap_or_default();
        let owner = session.owner_offset(&path.broadcast).unwrap_or(relay);
        let recv = f.recv_us as f64;
        let relay_us = relay.to_local(f.header.relay_us) as f64;
        let publisher_us = owner.to_local(f.header.publisher_us) as f64;
        let capture_us = owner.to_local(f.header.capture_us) as f64;
        let mut s = shared.lock().unwrap();
        let t = s.tracks.entry(path.clone()).or_default();
        t.kind = format!("{kind:?}").to_lowercase();
        t.frames += 1;
        t.groups.insert(f.group);
        t.bytes += f.payload.len() as u64;
        if t.codec != *f.codec {
            t.codec = f.codec.to_string();
        }
        t.publisher_to_relay.add((relay_us - publisher_us) / 1000.0);
        t.relay_to_probe.add((recv - relay_us) / 1000.0);
        // Video capture times are only real with the encoder's SEI.
        if kind == TrackKind::Data || f.header.flags & FLAG_CAPTURE_FROM_SEI != 0 {
            t.capture_to_probe.add((recv - capture_us) / 1000.0);
        }
    }
    Ok(())
}

/// Claims each broadcast's drive lock and drives it.
async fn teleop(session: Session, only: Option<String>, hz: f64, shared: Arc<Mutex<Shared>>) {
    let mut room = session.room();
    let locks: Vec<Path> = loop {
        let locks: Vec<Path> = room
            .borrow_and_update()
            .broadcasts
            .iter()
            .filter(|b| only.as_ref().is_none_or(|o| *o == b.name))
            .filter(|b| b.lock("drive").is_some())
            .map(|b| Path::new(&b.name, "drive"))
            .collect();
        if !locks.is_empty() {
            break locks;
        }
        if room.changed().await.is_err() {
            return;
        }
    };
    for lock in &locks {
        match session.claim(lock.clone(), true).await {
            Ok(Ok(())) => tracing::info!(%lock, "claimed"),
            Ok(Err((holder, reason))) => tracing::warn!(%lock, ?holder, %reason, "claim refused"),
            Err(e) => tracing::warn!(%lock, error = format!("{e:#}"), "claim failed"),
        }
    }
    {
        let mut replies = session.replies();
        let shared = shared.clone();
        tokio::spawn(async move {
            while let Ok(r) = replies.recv().await {
                let mut s = shared.lock().unwrap();
                match r {
                    Reply::Ack { rtt_us, status, .. } => {
                        s.ack_rtt.add(rtt_us as f64 / 1000.0);
                        *s.acks.entry(format!("{status:?}")).or_default() += 1;
                    }
                    Reply::Echo { rtt_us, tripped, .. } => {
                        s.heartbeat_rtt.add(rtt_us as f64 / 1000.0);
                        s.tripped = Some(tripped);
                    }
                }
            }
        });
    }
    let channels: Vec<_> = locks.iter().map(|l| session.channel(l.clone())).collect();
    let mut t = tokio::time::interval(Duration::from_secs_f64(1.0 / hz));
    let mut last_hb = Instant::now() - Duration::from_secs(1);
    loop {
        t.tick().await;
        for c in &channels {
            let _ = c.send_json(&serde_json::json!({"command": "drive", "args": {"throttle": 0.1, "steer": 0.0}}));
        }
        if last_hb.elapsed() >= Duration::from_millis(100) {
            last_hb = Instant::now();
            for l in &locks {
                let _ = session.heartbeat(l);
            }
        }
    }
}
