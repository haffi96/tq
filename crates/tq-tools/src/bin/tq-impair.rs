//! tq-impair: a UDP proxy that makes a link behave like Starlink (or any
//! other profile) without root: base delay, jitter, random and bursty loss,
//! a bandwidth cap with a bounded queue, and the periodic satellite handover
//! spikes. Put it between the publisher and the relay (or a browser and the
//! relay):
//!
//! ```text
//! tq-publisher --relay-url https://127.0.0.1:4444  ->  tq-impair :4444  ->  tq-relay :4443
//! ```
//!
//! "up" is client -> upstream (the vehicle uplink when the publisher is the
//! client), "down" is upstream -> client.

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use clap::{Parser, ValueEnum};
use rand::Rng;
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Mutex},
    time::Instant,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Profile {
    /// Pass-through.
    None,
    /// Good fibre: 5 ms, 1 ms jitter, no loss.
    Fibre,
    /// Starlink roaming/standard: ~25 ms each way, jittery, light random loss
    /// with occasional bursts, 10 Mbps up / 50 Mbps down, handover every 15 s.
    Starlink,
    /// A bad Starlink day: more loss, bigger handover spikes, 4 Mbps up.
    StarlinkBad,
}

#[derive(Parser, Debug)]
#[command(about = "UDP impairment proxy (Starlink-like links for QUIC testing)")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:4444")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:4443")]
    upstream: SocketAddr,
    #[arg(long, value_enum, default_value = "starlink")]
    profile: Profile,
    /// Override: one-way base delay, both directions (ms).
    #[arg(long)]
    delay_ms: Option<f64>,
    /// Override: jitter, both directions (ms, uniform +-).
    #[arg(long)]
    jitter_ms: Option<f64>,
    /// Override: random loss probability, both directions (0..1).
    #[arg(long)]
    loss: Option<f64>,
    /// Override: uplink rate (kbit/s, 0 = unlimited).
    #[arg(long)]
    up_kbps: Option<f64>,
    /// Override: downlink rate (kbit/s, 0 = unlimited).
    #[arg(long)]
    down_kbps: Option<f64>,
    /// Override: handover period (s, 0 = off).
    #[arg(long)]
    handover_every_s: Option<f64>,
    /// Override: handover duration (ms).
    #[arg(long)]
    handover_ms: Option<f64>,
    /// Override: extra one-way delay during a handover (ms).
    #[arg(long)]
    handover_extra_ms: Option<f64>,
    /// Override: loss probability during a handover.
    #[arg(long)]
    handover_loss: Option<f64>,
    /// Allow jitter to reorder packets (default keeps FIFO order, like a
    /// queue).
    #[arg(long)]
    reorder: bool,
    /// Seconds between stats lines.
    #[arg(long, default_value_t = 5)]
    stats_s: u64,
}

#[derive(Clone, Debug)]
struct Impairment {
    delay_ms: f64,
    jitter_ms: f64,
    loss: f64,
    /// Gilbert-Elliott: probability per packet of entering / leaving the
    /// bad state, and the loss probability in it.
    burst_enter: f64,
    burst_exit: f64,
    burst_loss: f64,
    kbps: f64,
    /// Tail-drop when the rate-limit queue holds more than this (ms).
    queue_ms: f64,
    handover_every_s: f64,
    handover_ms: f64,
    handover_extra_ms: f64,
    handover_loss: f64,
}

impl Impairment {
    fn profile(p: Profile, up: bool) -> Self {
        let none = Impairment {
            delay_ms: 0.0,
            jitter_ms: 0.0,
            loss: 0.0,
            burst_enter: 0.0,
            burst_exit: 1.0,
            burst_loss: 0.0,
            kbps: 0.0,
            queue_ms: 200.0,
            handover_every_s: 0.0,
            handover_ms: 0.0,
            handover_extra_ms: 0.0,
            handover_loss: 0.0,
        };
        match p {
            Profile::None => none,
            Profile::Fibre => Impairment {
                delay_ms: 5.0,
                jitter_ms: 1.0,
                ..none
            },
            Profile::Starlink => Impairment {
                delay_ms: 25.0,
                jitter_ms: 8.0,
                loss: 0.003,
                burst_enter: 0.001,
                burst_exit: 0.2,
                burst_loss: 0.5,
                kbps: if up { 10_000.0 } else { 50_000.0 },
                queue_ms: 150.0,
                handover_every_s: 15.0,
                handover_ms: 200.0,
                handover_extra_ms: 40.0,
                handover_loss: 0.05,
            },
            Profile::StarlinkBad => Impairment {
                delay_ms: 35.0,
                jitter_ms: 15.0,
                loss: 0.01,
                burst_enter: 0.003,
                burst_exit: 0.1,
                burst_loss: 0.7,
                kbps: if up { 4_000.0 } else { 25_000.0 },
                queue_ms: 250.0,
                handover_every_s: 15.0,
                handover_ms: 400.0,
                handover_extra_ms: 80.0,
                handover_loss: 0.2,
            },
        }
    }

    fn apply_overrides(&mut self, a: &Args, up: bool) {
        if let Some(v) = a.delay_ms {
            self.delay_ms = v;
        }
        if let Some(v) = a.jitter_ms {
            self.jitter_ms = v;
        }
        if let Some(v) = a.loss {
            self.loss = v;
        }
        if let Some(v) = if up { a.up_kbps } else { a.down_kbps } {
            self.kbps = v;
        }
        if let Some(v) = a.handover_every_s {
            self.handover_every_s = v;
        }
        if let Some(v) = a.handover_ms {
            self.handover_ms = v;
        }
        if let Some(v) = a.handover_extra_ms {
            self.handover_extra_ms = v;
        }
        if let Some(v) = a.handover_loss {
            self.handover_loss = v;
        }
    }
}

#[derive(Default)]
struct Stats {
    packets: AtomicU64,
    lost: AtomicU64,
    queue_drops: AtomicU64,
    delay_sum_us: AtomicU64,
}

/// One direction of the link: decides each packet's fate and release time.
struct Link {
    imp: Impairment,
    reorder: bool,
    start: Instant,
    bad: bool,
    link_free: Instant,
    last_release: Instant,
    stats: Arc<Stats>,
}

impl Link {
    fn new(imp: Impairment, reorder: bool, stats: Arc<Stats>) -> Self {
        let now = Instant::now();
        Self {
            imp,
            reorder,
            start: now,
            bad: false,
            link_free: now,
            last_release: now,
            stats,
        }
    }

    fn in_handover(&self, now: Instant) -> bool {
        if self.imp.handover_every_s <= 0.0 {
            return false;
        }
        let t = (now - self.start).as_secs_f64() % self.imp.handover_every_s;
        t < self.imp.handover_ms / 1000.0
    }

    /// Release time of a packet of `len` bytes, or None if it is lost.
    fn admit(&mut self, len: usize) -> Option<Instant> {
        let mut rng = rand::rng();
        let now = Instant::now();
        self.stats.packets.fetch_add(1, Relaxed);
        let handover = self.in_handover(now);

        // Gilbert-Elliott burst state.
        if self.bad {
            if rng.random::<f64>() < self.imp.burst_exit {
                self.bad = false;
            }
        } else if rng.random::<f64>() < self.imp.burst_enter {
            self.bad = true;
        }
        let p_loss = if self.bad { self.imp.burst_loss } else { self.imp.loss }
            + if handover { self.imp.handover_loss } else { 0.0 };
        if rng.random::<f64>() < p_loss {
            self.stats.lost.fetch_add(1, Relaxed);
            return None;
        }

        // Rate limit: serialize behind earlier packets, tail-drop when the
        // queue is too long.
        let mut depart = now;
        if self.imp.kbps > 0.0 {
            let ser = Duration::from_secs_f64(len as f64 * 8.0 / (self.imp.kbps * 1000.0));
            let start = self.link_free.max(now);
            if (start - now).as_secs_f64() * 1000.0 > self.imp.queue_ms {
                self.stats.queue_drops.fetch_add(1, Relaxed);
                return None;
            }
            self.link_free = start + ser;
            depart = self.link_free;
        }

        let mut delay = self.imp.delay_ms;
        if self.imp.jitter_ms > 0.0 {
            delay += rng.random_range(-self.imp.jitter_ms..=self.imp.jitter_ms);
        }
        if handover {
            delay += self.imp.handover_extra_ms;
        }
        let mut release = depart + Duration::from_secs_f64(delay.max(0.0) / 1000.0);
        if !self.reorder {
            release = release.max(self.last_release);
            self.last_release = release;
        }
        self.stats
            .delay_sum_us
            .fetch_add((release - now).as_micros() as u64, Relaxed);
        Some(release)
    }
}

/// Delivers packets at their release times, in order.
fn spawn_delivery(sock: Arc<UdpSocket>, to: Option<SocketAddr>) -> mpsc::UnboundedSender<(Instant, Vec<u8>)> {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    tokio::spawn(async move {
        let mut queue: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
        loop {
            let next = queue.front().map(|(t, _)| *t);
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(p) => {
                        // Keep sorted by release time (reorder mode can
                        // produce out-of-order times).
                        let pos = queue.iter().rposition(|(t, _)| *t <= p.0).map_or(0, |i| i + 1);
                        queue.insert(pos, p);
                    }
                    None => return,
                },
                _ = async { tokio::time::sleep_until(next.unwrap()).await }, if next.is_some() => {
                    let now = Instant::now();
                    while queue.front().is_some_and(|(t, _)| *t <= now) {
                        let (_, data) = queue.pop_front().unwrap();
                        let _ = match to {
                            Some(addr) => sock.send_to(&data, addr).await,
                            None => sock.send(&data).await,
                        };
                    }
                }
            }
        }
    });
    tx
}

struct Flow {
    up: Mutex<Link>,
    up_tx: mpsc::UnboundedSender<(Instant, Vec<u8>)>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let mut up = Impairment::profile(args.profile, true);
    up.apply_overrides(&args, true);
    let mut down = Impairment::profile(args.profile, false);
    down.apply_overrides(&args, false);
    tracing::info!(listen = %args.listen, upstream = %args.upstream, profile = ?args.profile, "impairment proxy");
    tracing::info!(?up, "up (client -> upstream)");
    tracing::info!(?down, "down (upstream -> client)");

    let listener = Arc::new(UdpSocket::bind(args.listen).await?);
    let flows: Arc<Mutex<HashMap<SocketAddr, Arc<Flow>>>> = Default::default();
    let up_stats = Arc::new(Stats::default());
    let down_stats = Arc::new(Stats::default());

    {
        let (up_stats, down_stats) = (up_stats.clone(), down_stats.clone());
        let every = Duration::from_secs(args.stats_s.max(1));
        tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.tick().await;
            loop {
                t.tick().await;
                for (name, s) in [("up", &up_stats), ("down", &down_stats)] {
                    let p = s.packets.swap(0, Relaxed);
                    let lost = s.lost.swap(0, Relaxed);
                    let q = s.queue_drops.swap(0, Relaxed);
                    let d = s.delay_sum_us.swap(0, Relaxed);
                    let delivered = p.saturating_sub(lost + q);
                    let mean = if delivered > 0 {
                        d as f64 / delivered as f64 / 1000.0
                    } else {
                        0.0
                    };
                    tracing::info!("{name:>4}: {p} pkts, {lost} lost, {q} queue drops, mean one-way {mean:.1} ms");
                }
            }
        });
    }

    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = listener.recv_from(&mut buf).await?;
        let flow = {
            let mut flows = flows.lock().await;
            match flows.get(&from) {
                Some(f) => f.clone(),
                None => {
                    // New client: its own upstream socket, so the upstream
                    // sees one address per client (like a NAT).
                    let upstream =
                        Arc::new(UdpSocket::bind(if args.upstream.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?);
                    upstream.connect(args.upstream).await?;
                    let up_tx = spawn_delivery(upstream.clone(), None);
                    let down_tx = spawn_delivery(listener.clone(), Some(from));
                    let mut down_link = Link::new(down.clone(), args.reorder, down_stats.clone());
                    let up_sock = upstream.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        while let Ok(n) = up_sock.recv(&mut buf).await {
                            if let Some(at) = down_link.admit(n) {
                                let _ = down_tx.send((at, buf[..n].to_vec()));
                            }
                        }
                    });
                    tracing::info!(client = %from, "new flow");
                    let f = Arc::new(Flow {
                        up: Mutex::new(Link::new(up.clone(), args.reorder, up_stats.clone())),
                        up_tx,
                    });
                    flows.insert(from, f.clone());
                    f
                }
            }
        };
        let release = flow.up.lock().await.admit(n);
        if let Some(at) = release {
            let _ = flow.up_tx.send((at, buf[..n].to_vec()));
        }
    }
}
