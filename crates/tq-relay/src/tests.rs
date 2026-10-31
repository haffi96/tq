//! End to end over QUIC on localhost: a relay, and sessions on the client
//! SDK publishing, subscribing, sending on channels and holding locks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, watch};
use tq::{Publication, Reply, Session};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::SendPolicy,
    quic::{self, QuicOptions, ServerTrust},
};
use tq_wire::{
    now_us, sign_token, AckStatus, Advertisement, ChannelInfo, Claims, Control, DataInfo, Delivery, Event,
    FrameHeader, LockChange, LockInfo, MediaFrame, Path, Perms, RoomEvent, RoomMessage, StopReason, Subscription,
    TrackInfo, VideoInfo, Watchdog, FLAG_KEY,
};

use crate::{
    session::{pump_events, EventsOut},
    Relay,
};

const SECRET: &str = "test-secret";
const GRACE: Duration = Duration::from_millis(300);
const WAIT: Duration = Duration::from_secs(3);

struct Harness {
    relay: Arc<Relay>,
    port: u16,
    hash: [u8; 32],
}

impl Harness {
    async fn start() -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .try_init();
        let cert = tq_transport::cert::self_signed(&["localhost".into(), "127.0.0.1".into()]).unwrap();
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut server = quic::server(
            ([127, 0, 0, 1], port).into(),
            cert.chain.clone(),
            cert.clone_key(),
            &QuicOptions::default(),
        )
        .unwrap();
        let policy = SendPolicy {
            keep_groups: 0,
            max_lag: Duration::from_millis(250),
        };
        let relay = Arc::new(Relay {
            build: tq_wire::build_info!("tq-relay"),
            secret: SECRET.into(),
            rooms: Mutex::new(HashMap::new()),
            track_grace: GRACE,
            cert_hash: cert.hash_hex(),
            self_signed: true,
            quic_port: port,
            dev: false,
            realtime_policy: policy,
            smooth_policy: policy,
        });
        let r = relay.clone();
        tokio::spawn(async move {
            while let Some(req) = server.accept().await {
                tokio::spawn(crate::handle(r.clone(), req));
            }
        });
        Self {
            relay,
            port,
            hash: cert.sha256,
        }
    }

    fn token(&self, sub: &str, perms: Perms) -> String {
        sign_token(
            SECRET.as_bytes(),
            &Claims {
                sub: sub.into(),
                room: "r1".into(),
                exp: now_us() / 1_000_000 + 60,
                perms,
            },
        )
    }

    fn options(&self, sub: &str, perms: Perms) -> tq::Options {
        tq::Options {
            relay_url: format!("https://127.0.0.1:{}", self.port).parse().unwrap(),
            room: "r1".into(),
            token: self.token(sub, perms),
            trust: ServerTrust::Hashes(vec![self.hash]),
            quic: QuicOptions::default(),
            build: tq_wire::build_info!("tq-test"),
        }
    }

    async fn session(&self, sub: &str, perms: Perms) -> Session {
        Session::connect(self.options(sub, perms)).await.unwrap()
    }

    /// A raw WebTransport session, for protocol misbehaviour.
    async fn raw(&self, sub: &str) -> tq_transport::Session {
        let client = quic::client(ServerTrust::Hashes(vec![self.hash]), &QuicOptions::default()).unwrap();
        let mut url = url::Url::parse(&format!("https://127.0.0.1:{}/v1/room/r1", self.port)).unwrap();
        url.query_pairs_mut()
            .append_pair("token", &self.token(sub, Perms::viewer()));
        client.connect(url).await.unwrap()
    }
}

fn data(id: &str, delivery: Delivery) -> TrackInfo {
    TrackInfo::data(
        id,
        DataInfo {
            encoding: "json".into(),
            delivery,
            rate_hz: None,
            window_ms: None,
        },
    )
}

fn video(id: &str) -> TrackInfo {
    TrackInfo::video(
        id,
        VideoInfo {
            codec: "h264".into(),
            width: 640,
            height: 480,
            fps: 30,
        },
    )
}

fn rover(tracks: Vec<TrackInfo>) -> Advertisement {
    Advertisement {
        broadcast: "rover".into(),
        tracks,
        channels: vec![
            ChannelInfo {
                id: "drive".into(),
                label: String::new(),
                lock: Some("drive".into()),
                encoding: "json".into(),
                schema: None,
            },
            ChannelInfo {
                id: "estop".into(),
                label: String::new(),
                lock: None,
                encoding: "json".into(),
                schema: None,
            },
        ],
        locks: vec![LockInfo {
            id: "drive".into(),
            label: String::new(),
            watchdog: Some(Watchdog {
                heartbeat_loss_ms: 400,
                message_stale_ms: 400,
            }),
        }],
    }
}

fn path(s: &str) -> Path {
    s.parse().unwrap()
}

/// The first event `f` accepts, skipping the rest.
async fn expect<T>(rx: &mut broadcast::Receiver<RoomEvent>, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let e = rx.recv().await.unwrap();
            if let Some(t) = f(&e.event) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

/// Waits until the room state satisfies `f`.
async fn until(s: &Session, f: impl Fn(&tq_wire::RoomState) -> bool) {
    let mut room = s.room();
    tokio::time::timeout(WAIT, room.wait_for(|r| f(r)))
        .await
        .expect("timed out waiting for the room state")
        .unwrap();
}

async fn frame(r: &mut tq::TrackReader) -> tq::Frame {
    tokio::time::timeout(WAIT, r.recv())
        .await
        .expect("timed out waiting for a frame")
        .unwrap()
}

async fn reply(rx: &mut broadcast::Receiver<Reply>) -> Reply {
    tokio::time::timeout(WAIT, rx.recv()).await.expect("no reply").unwrap()
}

#[tokio::test]
async fn publish_and_subscribe() {
    let h = Harness::start().await;
    let viewer = h.session("viewer", Perms::viewer()).await;
    let mut events = viewer.events();
    let robot = h.session("robot", Perms::robot("rover")).await;
    let (p, _inbox) = Publication::new(rover(vec![
        video("cam"),
        data("pose", Delivery::Latest),
        data("log", Delivery::Stream),
        data("imu", Delivery::Datagram),
    ]))
    .unwrap();
    robot.publish(&p).await.unwrap();
    expect(&mut events, |e| match e {
        Event::BroadcastStarted { broadcast, .. } if broadcast == "rover" => Some(()),
        _ => None,
    })
    .await;
    for id in ["cam", "pose", "log", "imu"] {
        p.set_online(id, true, "");
    }
    expect(&mut events, |e| matches!(e, Event::TrackOnline { track } if track.name == "imu").then_some(())).await;

    // Latest: subscribers get the newest value, late ones at once.
    let mut pose = viewer.subscribe(path("rover/pose")).await.unwrap();
    let mut demand = p.demand();
    demand.wait_for(|d| d.get("pose") == Some(&1)).await.unwrap();
    let w = p.data("pose").unwrap();
    w.send_json(&serde_json::json!({ "x": 1 }));
    let f = frame(&mut pose).await;
    assert_eq!(f.json::<serde_json::Value>().unwrap()["x"], 1);
    assert!(f.header.relay_us >= f.header.publisher_us && f.header.is_key());
    assert_eq!(&*f.codec, "json");
    let late = h.session("late", Perms::viewer()).await;
    let mut late_pose = late.subscribe(path("rover/pose")).await.unwrap();
    assert_eq!(frame(&mut late_pose).await.json::<serde_json::Value>().unwrap()["x"], 1);

    // Stream: every message, in order.
    let mut log = viewer.subscribe(path("rover/log")).await.unwrap();
    demand.wait_for(|d| d.get("log") == Some(&1)).await.unwrap();
    let w = p.data("log").unwrap();
    for i in 0..20 {
        w.send_json(&i);
    }
    for i in 0..20 {
        assert_eq!(frame(&mut log).await.json::<i32>().unwrap(), i);
    }

    // Datagram tracks.
    let mut imu = viewer.subscribe(path("rover/imu")).await.unwrap();
    demand.wait_for(|d| d.get("imu") == Some(&1)).await.unwrap();
    let w = p.data("imu").unwrap();
    w.send_json(&7);
    let f = frame(&mut imu).await;
    assert_eq!(f.json::<i32>().unwrap(), 7);
    assert_eq!((f.group, f.header.frame_id), (0, 1));
    assert!(f.header.relay_us > 0);

    // Video: GOPs, frames in order. A new GOP supersedes the last at once
    // on a realtime uplink, so each is sent after the previous arrived.
    let mut cam = viewer.subscribe(path("rover/cam")).await.unwrap();
    demand.wait_for(|d| d.get("cam") == Some(&1)).await.unwrap();
    let t = p.video("cam").unwrap();
    let push = |group, index| {
        t.push(MediaFrame {
            broadcast: 0,
            track: 0,
            group,
            codec: "avc1.42c01f".into(),
            header: FrameHeader {
                index,
                flags: if index == 0 { FLAG_KEY } else { 0 },
                capture_us: now_us(),
                publisher_us: now_us(),
                relay_us: 0,
                frame_id: index,
            },
            payload: Bytes::from_static(b"au"),
        })
    };
    for index in 0..3 {
        push(1, index);
    }
    let mut got = Vec::new();
    for _ in 0..3 {
        let f = frame(&mut cam).await;
        got.push((f.group, f.header.index));
    }
    push(2, 0);
    let f = frame(&mut cam).await;
    got.push((f.group, f.header.index));
    let g = got[0].0;
    assert_eq!(got, [(g, 0), (g, 1), (g, 2), (g + 1, 0)]);
    until(&viewer, |r| {
        r.track(&path("rover/cam"))
            .is_some_and(|t| t.stats.codec_string.as_deref() == Some("avc1.42c01f"))
    })
    .await;

    // Dropping a reader unsubscribes; the owner sees demand fall.
    drop(cam);
    demand.wait_for(|d| d.get("cam") == Some(&0)).await.unwrap();

    // The relay counts what arrives, and the state shows the owner's clock.
    until(&viewer, |r| {
        r.track(&path("rover/log")).is_some_and(|t| t.stats.frames == 20 && t.stats.subscribers == 1)
    })
    .await;
    assert!(viewer.owner_offset("rover").is_some());
}

#[tokio::test]
async fn permissions_are_per_path() {
    let h = Harness::start().await;
    let robot = h.session("robot", Perms::robot("rover")).await;
    let (p, _inbox) = Publication::new(rover(vec![
        data("pose", Delivery::Latest),
        data("secret", Delivery::Latest),
    ]))
    .unwrap();
    robot.publish(&p).await.unwrap();

    let narrow = h.session("narrow", Perms::new(["sub:rover/pose"])).await;
    assert!(narrow.subscribe(path("rover/pose")).await.is_ok());
    let e = narrow.subscribe(path("rover/secret")).await.err().unwrap();
    assert!(format!("{e:#}").contains("not allowed"), "{e:#}");

    // Publishing needs `pub:` for that name.
    let (other, _) = Publication::new(Advertisement {
        broadcast: "other".into(),
        ..Default::default()
    })
    .unwrap();
    let e = robot.publish(&other).await.unwrap_err();
    assert!(format!("{e:#}").contains("not allowed"), "{e:#}");
    assert!(narrow.claim(path("rover/drive"), false).await.unwrap().is_err());
}

#[tokio::test]
async fn subscriptions_are_thinned_to_max_rate() {
    let h = Harness::start().await;
    let robot = h.session("robot", Perms::robot("rover")).await;
    let (p, _inbox) = Publication::new(rover(vec![data("imu", Delivery::Datagram)])).unwrap();
    robot.publish(&p).await.unwrap();
    let viewer = h.session("viewer", Perms::viewer()).await;
    let mut imu = viewer
        .subscribe(Subscription {
            track: path("rover/imu"),
            max_rate_hz: Some(10.0),
            policy: Default::default(),
        })
        .await
        .unwrap();
    p.demand().wait_for(|d| d.get("imu") == Some(&1)).await.unwrap();
    let w = p.data("imu").unwrap();
    // 100 Hz for half a second.
    for i in 0..50 {
        w.send_json(&i);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut got = Vec::new();
    while let Some(f) = imu.try_recv() {
        got.push(f.json::<i32>().unwrap());
    }
    assert!((3..=9).contains(&got.len()), "{got:?}");
    assert_eq!(got.last(), Some(&49), "the newest always gets through: {got:?}");
}

#[tokio::test]
async fn channels_locks_and_watchdog() {
    let h = Harness::start().await;
    let robot = h.session("robot", Perms::robot("rover")).await;
    let (p, mut inbox) = Publication::new(rover(vec![])).unwrap();
    robot.publish(&p).await.unwrap();
    let mut holders = p.holders();
    let mut wd = p.watchdog("drive").unwrap();

    let op1 = h.session("op1", Perms::operator()).await;
    let op2 = h.session("op2", Perms::operator()).await;
    let viewer = h.session("viewer", Perms::viewer()).await;
    for s in [&op1, &op2, &viewer] {
        until(s, |r| r.broadcast("rover").is_some()).await;
    }
    let mut replies1 = op1.replies();

    // An open channel: anyone with `write:` reaches the owner, who knows
    // who sent it; the ack goes back to the sender alone.
    op1.channel(path("rover/estop")).send_json(&"stop").unwrap();
    let m = tokio::time::timeout(WAIT, inbox.recv()).await.unwrap().unwrap();
    assert_eq!((m.channel.as_str(), m.from_sid, m.json::<String>().unwrap().as_str()), ("estop", op1.sid(), "stop"));
    m.accept();
    let Reply::Ack { channel, status, .. } = reply(&mut replies1).await else { panic!() };
    assert_eq!((channel, status), (path("rover/estop"), AckStatus::Ok));

    // No `write:`, or the channel's lock not held: dropped by the relay.
    viewer.channel(path("rover/estop")).send_json(&"stop").unwrap();
    op1.channel(path("rover/drive")).send_json(&1).unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), inbox.recv()).await.is_err());

    // Claiming: the owner hears who holds the lock; its watchdog stays
    // tripped until an accepted message.
    assert!(op1.claim(path("rover/drive"), false).await.unwrap().is_ok());
    holders
        .wait_for(|h| h["drive"].as_ref().is_some_and(|h| h.sid == op1.sid()))
        .await
        .unwrap();
    assert!(wd.borrow().tripped);
    op1.heartbeat(&path("rover/drive")).unwrap();
    let Reply::Echo { tripped, .. } = reply(&mut replies1).await else { panic!() };
    assert!(tripped);
    let drive = op1.channel(path("rover/drive"));
    drive.send_json(&serde_json::json!({ "throttle": 0.5 })).unwrap();
    let m = tokio::time::timeout(WAIT, inbox.recv()).await.unwrap().unwrap();
    assert_eq!(m.lock.as_deref(), Some("drive"));
    m.accept();
    wd.wait_for(|s| !s.tripped).await.unwrap();

    // A stale sequence number is superseded without reaching the app.
    let raw = tq_wire::Datagram::Message {
        sid: 0,
        broadcast: op1.room().borrow().broadcast("rover").unwrap().index,
        channel: 0,
        seq: 1,
        sent_us: now_us(),
        payload: Bytes::from_static(b"1"),
    };
    let _ = reply(&mut replies1).await;
    op1.transport().send_datagram(raw.encode()).unwrap();
    let Reply::Ack { status, .. } = reply(&mut replies1).await else { panic!() };
    assert_eq!(status, AckStatus::Superseded);

    // Without heartbeats or messages the watchdog trips.
    wd.wait_for(|s| s.tripped).await.unwrap();
    assert_eq!(wd.borrow().reason, "holder heartbeat lost");

    // Another operator: refused without force, takes over with it.
    let mut events = viewer.events();
    let (holder, _) = op2.claim(path("rover/drive"), false).await.unwrap().unwrap_err();
    assert_eq!(holder.as_deref(), Some(op1.id()));
    assert!(op2.claim(path("rover/drive"), true).await.unwrap().is_ok());
    let reason = expect(&mut events, |e| match e {
        Event::LockChanged { reason, .. } => Some(*reason),
        _ => None,
    })
    .await;
    assert_eq!(reason, LockChange::Forced);
    holders
        .wait_for(|h| h["drive"].as_ref().is_some_and(|h| h.sid == op2.sid()))
        .await
        .unwrap();

    // The holder leaves: the lock is free.
    op2.close();
    holders.wait_for(|h| h["drive"].is_none()).await.unwrap();
    let reason = expect(&mut events, |e| match e {
        Event::LockChanged { reason, .. } => Some(*reason),
        _ => None,
    })
    .await;
    assert_eq!(reason, LockChange::Left);
}

#[tokio::test]
async fn outage_takeover_and_removal() {
    let h = Harness::start().await;
    let viewer = h.session("viewer", Perms::viewer()).await;
    let mut events = viewer.events();

    let r1 = h.session("robot", Perms::robot("rover")).await;
    let (p, _inbox) = Publication::new(rover(vec![data("a", Delivery::Latest), data("b", Delivery::Latest)])).unwrap();
    r1.publish(&p).await.unwrap();
    p.set_online("a", true, "");
    expect(&mut events, |e| matches!(e, Event::TrackOnline { track } if track.name == "a").then_some(())).await;

    // The owner drops: its online track goes offline, the broadcast stays.
    r1.close();
    expect(&mut events, |e| matches!(e, Event::TrackOffline { track, .. } if track.name == "a").then_some(())).await;
    let reason = expect(&mut events, |e| match e {
        Event::BroadcastStopped { reason, .. } => Some(*reason),
        _ => None,
    })
    .await;
    assert_eq!(reason, StopReason::Disconnected);

    // It comes back within the grace period with `b` gone and `c` new.
    let r2 = h.session("robot", Perms::robot("rover")).await;
    let (p2, _inbox2) = Publication::new(rover(vec![data("a", Delivery::Latest), data("c", Delivery::Latest)])).unwrap();
    r2.publish(&p2).await.unwrap();
    expect(&mut events, |e| matches!(e, Event::BroadcastStarted { .. }).then_some(())).await;
    expect(&mut events, |e| matches!(e, Event::TrackUnpublished { track } if track.name == "b").then_some(())).await;
    expect(&mut events, |e| matches!(e, Event::TrackPublished { track, .. } if track.name == "c").then_some(())).await;

    // A second session takes over: the first one is closed.
    let r3 = h.session("robot", Perms::robot("rover")).await;
    r3.publish(&p2).await.unwrap();
    let reason = expect(&mut events, |e| match e {
        Event::BroadcastStopped { reason, .. } => Some(*reason),
        _ => None,
    })
    .await;
    assert_eq!(reason, StopReason::Replaced);
    let why = tokio::time::timeout(WAIT, r2.closed()).await.unwrap();
    assert!(why.contains("replaced"), "{why}");

    // It drops for good: after the grace period the broadcast is removed.
    r3.close();
    expect(&mut events, |e| matches!(e, Event::BroadcastRemoved { .. }).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let room = h.relay.rooms.lock().unwrap()["r1"].clone();
    assert!(room.state().broadcasts.is_empty());
    until(&viewer, |r| r.broadcasts.is_empty() && r.participants.len() == 1).await;
}

#[tokio::test]
async fn hello_is_required() {
    let h = Harness::start().await;
    let session = h.raw("rude").await;
    let (send, recv) = session.open_bi().await.unwrap();
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    ctl.send(&Control::Release { lock: path("a/b") }).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
    assert!(
        !matches!(r, Ok(Some(Control::Welcome { .. }))),
        "welcomed without a hello: {r:?}"
    );
}

#[tokio::test]
async fn other_protocol_majors_are_refused() {
    let h = Harness::start().await;
    let mut opts = h.options("old", Perms::viewer());
    opts.build.protocol.major -= 1;
    let e = Session::connect(opts).await.err().expect("connected with another major");
    assert!(format!("{e:#}").contains("protocol"), "{e:#}");
}

#[tokio::test]
async fn rooms_api_needs_a_token_for_the_room() {
    let h = Harness::start().await;
    let _v = h.session("watcher", Perms::viewer()).await;
    let token = |room: &str| {
        sign_token(
            SECRET.as_bytes(),
            &Claims {
                sub: "ops".into(),
                room: room.into(),
                exp: now_us() / 1_000_000 + 60,
                perms: Perms::viewer(),
            },
        )
    };
    assert!(crate::web::rooms_for(&h.relay, "").is_err());
    assert!(crate::web::rooms_for(&h.relay, "not.a.token").is_err());
    assert!(crate::web::rooms_for(&h.relay, &token("other")).unwrap().is_empty());
    let rooms = crate::web::rooms_for(&h.relay, &token("r1")).unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].participants[0].sub, "watcher");
    assert_eq!(crate::web::rooms_for(&h.relay, &token("*")).unwrap().len(), 1);
}

/// Collects messages; blocks every send after the first until `open`.
struct Gated {
    open: watch::Receiver<bool>,
    sent: usize,
    out: mpsc::UnboundedSender<RoomMessage>,
}

impl EventsOut for Gated {
    async fn send(&mut self, bytes: &Bytes) -> anyhow::Result<()> {
        if self.sent > 0 {
            self.open.wait_for(|o| *o).await?;
        }
        self.sent += 1;
        self.out.send(RoomMessage::decode(&bytes[4..])?)?;
        Ok(())
    }
}

fn seq(m: &RoomMessage) -> u64 {
    match m {
        RoomMessage::State { seq, .. } => *seq,
        RoomMessage::Event(e) => e.seq,
        RoomMessage::Unknown => unreachable!(),
    }
}

#[tokio::test]
async fn events_in_order_and_resync_after_lag() {
    let room = crate::room::Room::new("r".into(), Duration::from_secs(30));
    let event = || Event::BroadcastRemoved {
        broadcast: "x".into(),
    };
    room.emit_for_test(event());
    let (open_tx, open) = watch::channel(false);
    let (out, mut rx) = mpsc::unbounded_channel();
    let pump = {
        let room = room.clone();
        tokio::spawn(async move { pump_events(&room, &mut Gated { open, sent: 0, out }).await })
    };
    // The first state, as of the first event.
    let first = rx.recv().await.unwrap();
    assert!(matches!(first, RoomMessage::State { seq: 1, .. }));

    // One event, then far more than the buffer while the session is stuck
    // on it.
    room.emit_for_test(event());
    tokio::time::sleep(Duration::from_millis(20)).await;
    for _ in 0..400 {
        room.emit_for_test(event());
    }
    let last = 402;
    open_tx.send(true).unwrap();

    let mut got = vec![first];
    while got.last().map(seq) != Some(last) {
        got.push(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap());
    }
    pump.abort();

    // Sequence numbers never go back, events follow on from the last state
    // or event without gaps, and the lag was covered by a state.
    let mut at = 0;
    let mut resynced = false;
    for m in &got {
        match m {
            RoomMessage::State { seq, .. } => {
                assert!(*seq >= at, "state {seq} after {at}");
                resynced |= *seq > 2;
                at = *seq;
            }
            RoomMessage::Event(e) => {
                assert_eq!(e.seq, at + 1, "gap before event {}", e.seq);
                at = e.seq;
            }
            RoomMessage::Unknown => unreachable!(),
        }
    }
    assert!(resynced, "no resync state: {} messages", got.len());
    assert!(got.len() < 100, "lagged session replayed everything: {} messages", got.len());
}
