//! tq-probe: a headless browser stand-in. Joins the relay as a viewer or
//! operator, receives every camera's GOP streams and reports per-hop
//! latency; as an operator it can claim and stream teleop commands to
//! exercise the dead-man path.
//!
//! ```text
//! tq-probe --relay https://127.0.0.1:4443 --av robot-01 --role operator --teleop-hz 20 --duration 20
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use tq_proto::{clock_offset, now_us, sign_token, Claims, Control, Datagram, Role, FLAG_CAPTURE_FROM_SEI};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::GroupReader,
    quic::{self, QuicOptions, ServerTrust},
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "https://127.0.0.1:4443")]
    relay: String,
    /// Fetch the relay certificate hash from here (plain HTTP).
    #[arg(long, default_value = "http://127.0.0.1:8080/api/cert")]
    cert_url: String,
    #[arg(long, default_value = "robot-01")]
    av: String,
    #[arg(long, default_value = "viewer")]
    role: Role,
    #[arg(long, env = "TQ_SHARED_SECRET", hide_env_values = true)]
    secret: String,
    /// Seconds to run (0 = forever).
    #[arg(long, default_value_t = 10)]
    duration: u64,
    /// Operator only: claim (force) and send teleop commands at this rate.
    #[arg(long, default_value_t = 0.0)]
    teleop_hz: f64,
    /// Print one summary line as JSON at the end.
    #[arg(long)]
    json: bool,
    /// Skip the `hello` (behave like a client from before protocol 1.1).
    #[arg(long)]
    no_hello: bool,
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
        serde_json::json!({
            "n": v.len(),
            "mean": (v.iter().sum::<f64>() / v.len() as f64 * 10.0).round() / 10.0,
            "p50": (pct(0.5) * 10.0).round() / 10.0,
            "p95": (pct(0.95) * 10.0).round() / 10.0,
            "max": (pct(1.0) * 10.0).round() / 10.0,
        })
    }
}

#[derive(Default)]
struct CameraStats {
    frames: u64,
    groups: u64,
    bytes: u64,
    capture_to_probe: Samples,
    publisher_to_relay: Samples,
    relay_to_probe: Samples,
    codec: String,
}

struct Shared {
    cameras: HashMap<u16, CameraStats>,
    /// Probe clock minus relay clock (us).
    my_offset: i64,
    clock: tq_proto::ClockFilter,
    /// Publisher clock minus relay clock (us).
    pub_offset: i64,
    teleop_rtt: Samples,
    heartbeat_rtt: Samples,
    acks: HashMap<String, u64>,
    tripped: Option<bool>,
    names: Vec<String>,
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
    let claims = Claims {
        sub: format!("probe-{}", args.role),
        role: args.role,
        av: args.av.clone(),
        exp: now_us() / 1_000_000 + 3600,
    };
    let token = sign_token(args.secret.as_bytes(), &claims);
    let hash = fetch_hash(&args.cert_url).await?;
    let client = quic::client(ServerTrust::Hashes(vec![hash]), &QuicOptions::default())?;
    let mut url = url::Url::parse(&args.relay)?;
    url.set_path(&format!("/v1/session/{}", args.av));
    url.query_pairs_mut().append_pair("token", &token);
    let session = client.connect(url).await.context("connect")?;
    let (send, recv) = session.open_bi().await?;
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    if !args.no_hello {
        let mut build = tq_proto::build_info!("tq-probe");
        if let Some(p) = &args.fake_protocol {
            let (major, minor) = p.split_once('.').context("--fake-protocol MAJOR.MINOR")?;
            build.protocol = tq_proto::ProtocolVersion {
                major: major.parse()?,
                minor: minor.parse()?,
            };
        }
        ctl.send(&Control::Hello { build }).await?;
    }
    let shared = Arc::new(Mutex::new(Shared {
        cameras: HashMap::new(),
        my_offset: 0,
        clock: tq_proto::ClockFilter::new(20),
        pub_offset: 0,
        teleop_rtt: Samples::default(),
        heartbeat_rtt: Samples::default(),
        acks: HashMap::new(),
        tripped: None,
        names: Vec::new(),
    }));

    // Clock sync with the relay.
    {
        let ctl = ctl.clone();
        tokio::spawn(async move {
            let mut id = 0;
            loop {
                id += 1;
                if ctl.send(&Control::Ping { id, t0_us: now_us() }).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }
    // Control messages.
    {
        let shared = shared.clone();
        let ctl = ctl.clone();
        let operator = args.role == Role::Operator && args.teleop_hz > 0.0;
        tokio::spawn(async move {
            while let Ok(Some(msg)) = rx.recv().await {
                match msg {
                    Control::Welcome {
                        session,
                        role,
                        state,
                        relay,
                    } => {
                        let relay = relay.map_or_else(|| "unknown (protocol 1.0)".to_string(), |b| b.long());
                        let publisher = state.publisher.as_ref().map(|p| {
                            p.build
                                .as_ref()
                                .map_or_else(|| "unknown (protocol 1.0)".to_string(), |b| b.long())
                        });
                        tracing::info!(%session, %role, %relay, ?publisher, "welcome");
                        if operator {
                            let _ = ctl.send(&Control::Claim { force: true }).await;
                        }
                        if let Some(p) = state.publisher {
                            shared.lock().unwrap().names = p.cameras.iter().map(|c| c.id.clone()).collect();
                        }
                    }
                    Control::State { state } => {
                        let mut s = shared.lock().unwrap();
                        if let Some(p) = state.publisher {
                            s.pub_offset = p.clock_offset_us.unwrap_or(0);
                            s.names = p.cameras.iter().map(|c| c.id.clone()).collect();
                            s.tripped = Some(p.teleop.deadman_tripped);
                        }
                    }
                    Control::Pong { t0_us, t1_us, .. } => {
                        // offset of relay relative to me; mine relative to relay is the negative.
                        let (relay_minus_me, rtt) = clock_offset(t0_us, t1_us, now_us());
                        let mut s = shared.lock().unwrap();
                        s.clock.add(-relay_minus_me, rtt);
                        s.my_offset = s.clock.estimate().map_or(0, |(o, _)| o);
                    }
                    Control::ClaimResult { ok, holder, reason } => tracing::info!(ok, ?holder, ?reason, "claim"),
                    _ => {}
                }
            }
        });
    }
    // Teleop.
    if args.role == Role::Operator && args.teleop_hz > 0.0 {
        let session2 = session.clone();
        let period = Duration::from_secs_f64(1.0 / args.teleop_hz);
        tokio::spawn(async move {
            let mut seq = 0u32;
            let mut hb = 0u32;
            let mut t = tokio::time::interval(period);
            let mut last_hb = Instant::now();
            loop {
                t.tick().await;
                seq += 1;
                let body = serde_json::to_vec(
                    &serde_json::json!({"command": "drive", "args": {"throttle": 0.1, "steer": 0.0}}),
                )
                .unwrap();
                let _ = session2.send_datagram(
                    Datagram::Command {
                        seq,
                        sent_us: now_us(),
                        body: body.into(),
                    }
                    .encode(),
                );
                if last_hb.elapsed() >= Duration::from_millis(100) {
                    hb += 1;
                    last_hb = Instant::now();
                    let _ = session2.send_datagram(
                        Datagram::Heartbeat {
                            seq: hb,
                            sent_us: now_us(),
                        }
                        .encode(),
                    );
                }
            }
        });
        let shared = shared.clone();
        let session2 = session.clone();
        tokio::spawn(async move {
            while let Ok(d) = session2.read_datagram().await {
                let now = now_us();
                let mut s = shared.lock().unwrap();
                match Datagram::decode(d) {
                    Ok(Datagram::Ack { echo_us, status, .. }) => {
                        s.teleop_rtt.add(now.saturating_sub(echo_us) as f64 / 1000.0);
                        *s.acks.entry(format!("{status:?}")).or_default() += 1;
                    }
                    Ok(Datagram::Echo { echo_us, tripped, .. }) => {
                        s.heartbeat_rtt.add(now.saturating_sub(echo_us) as f64 / 1000.0);
                        s.tripped = Some(tripped);
                    }
                    _ => {}
                }
            }
        });
    }
    // Video.
    {
        let shared = shared.clone();
        let session = session.clone();
        tokio::spawn(async move {
            while let Ok(stream) = session.accept_uni().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let Ok(mut reader) = GroupReader::open(stream).await else {
                        return;
                    };
                    let cam = reader.header.camera;
                    {
                        let mut s = shared.lock().unwrap();
                        let c = s.cameras.entry(cam).or_default();
                        c.groups += 1;
                        c.codec = reader.header.codec.clone();
                    }
                    while let Ok(Some((h, payload))) = reader.next().await {
                        let now = now_us() as i64;
                        let mut s = shared.lock().unwrap();
                        let (me, pubo) = (s.my_offset, s.pub_offset);
                        let c = s.cameras.entry(cam).or_default();
                        c.frames += 1;
                        c.bytes += payload.len() as u64;
                        // Everything in relay clock.
                        let now_r = now - me;
                        let relay = h.relay_us as i64;
                        let publ = h.publisher_us as i64 - pubo;
                        let cap = h.capture_us as i64 - pubo;
                        c.publisher_to_relay.add((relay - publ) as f64 / 1000.0);
                        c.relay_to_probe.add((now_r - relay) as f64 / 1000.0);
                        if h.flags & FLAG_CAPTURE_FROM_SEI != 0 {
                            c.capture_to_probe.add((now_r - cap) as f64 / 1000.0);
                        }
                    }
                });
            }
        });
    }

    let started = Instant::now();
    let mut report = tokio::time::interval(Duration::from_secs(2));
    report.tick().await;
    loop {
        tokio::select! {
            _ = report.tick() => {
                let s = shared.lock().unwrap();
                for (cam, c) in &s.cameras {
                    let name = s.names.get(*cam as usize).cloned().unwrap_or_else(|| cam.to_string());
                    tracing::info!(
                        "{name}: {} frames {} groups {} | capture->probe {} | pub->relay {} | relay->probe {}",
                        c.frames, c.groups, c.codec,
                        c.capture_to_probe.summary(), c.publisher_to_relay.summary(), c.relay_to_probe.summary()
                    );
                }
                if args.teleop_hz > 0.0 {
                    tracing::info!("teleop rtt {} heartbeat rtt {} acks {:?} tripped {:?}", s.teleop_rtt.summary(), s.heartbeat_rtt.summary(), s.acks, s.tripped);
                }
            }
            e = session.closed() => anyhow::bail!("session closed: {e}"),
        }
        if args.duration > 0 && started.elapsed() >= Duration::from_secs(args.duration) {
            break;
        }
    }
    let s = shared.lock().unwrap();
    let secs = started.elapsed().as_secs_f64();
    let cams: serde_json::Map<String, serde_json::Value> = s
        .cameras
        .iter()
        .map(|(cam, c)| {
            let name = s.names.get(*cam as usize).cloned().unwrap_or_else(|| cam.to_string());
            (
                name,
                serde_json::json!({
                    "codec": c.codec, "frames": c.frames, "groups": c.groups,
                    "fps": (c.frames as f64 / secs * 10.0).round() / 10.0,
                    "kbps": (c.bytes as f64 * 8.0 / 1000.0 / secs).round(),
                    "capture_to_probe_ms": c.capture_to_probe.summary(),
                    "publisher_to_relay_ms": c.publisher_to_relay.summary(),
                    "relay_to_probe_ms": c.relay_to_probe.summary(),
                }),
            )
        })
        .collect();
    let out = serde_json::json!({
        "cameras": cams,
        "teleop_rtt_ms": s.teleop_rtt.summary(),
        "heartbeat_rtt_ms": s.heartbeat_rtt.summary(),
        "acks": s.acks,
        "deadman_tripped": s.tripped,
        "quic": tq_transport::quic::quic_stats(&session),
    });
    if args.json {
        println!("{out}");
    } else {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    session.close(0, b"done");
    Ok(())
}

async fn fetch_hash(url: &str) -> anyhow::Result<[u8; 32]> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let u = url::Url::parse(url)?;
    let host = u.host_str().context("no host")?;
    let mut s = tokio::net::TcpStream::connect((host, u.port().unwrap_or(80))).await?;
    s.write_all(format!("GET {} HTTP/1.0\r\nHost: {host}\r\n\r\n", u.path()).as_bytes())
        .await?;
    let mut body = Vec::new();
    s.read_to_end(&mut body).await?;
    let text = String::from_utf8_lossy(&body);
    let json: serde_json::Value = serde_json::from_str(text.split("\r\n\r\n").nth(1).context("bad response")?)?;
    tq_transport::cert::parse_hash(json["sha256"].as_str().context("no sha256")?)
}
