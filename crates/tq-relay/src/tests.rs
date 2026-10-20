//! End to end over QUIC on localhost: a relay, a stand-in publisher and
//! operator, and a viewer that checks the room's events.

use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc, Mutex},
    time::Duration,
};

use tq_proto::{
    now_us, sign_token, CameraInfo, Claims, Control, Event, OperatorChange, PublisherGone, Role, RoomEvent,
    RoomMessage, RoomState,
};
use tq_transport::{
    control::{ControlRx, ControlTx},
    events::{EventsRx, EventsTx, Incoming},
    group::SendPolicy,
    quic::{self, QuicOptions, ServerTrust},
    Session,
};

use crate::Relay;

const SECRET: &str = "test-secret";
const GRACE: Duration = Duration::from_millis(300);

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
            build: tq_proto::build_info!("tq-relay"),
            secret: SECRET.into(),
            rooms: Mutex::new(HashMap::new()),
            track_grace: GRACE,
            cert_hash: cert.hash_hex(),
            self_signed: true,
            quic_port: port,
            dev: false,
            operator_policy: policy,
            viewer_policy: policy,
            next_conn: AtomicU64::new(1),
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

    async fn connect(&self, kind: &str, role: Role, sub: &str) -> Session {
        let token = sign_token(
            SECRET.as_bytes(),
            &Claims {
                sub: sub.into(),
                role,
                room: "r1".into(),
                exp: now_us() / 1_000_000 + 60,
            },
        );
        let client = quic::client(ServerTrust::Hashes(vec![self.hash]), &QuicOptions::default()).unwrap();
        let mut url = url::Url::parse(&format!("https://127.0.0.1:{}/v1/{kind}/r1", self.port)).unwrap();
        url.query_pairs_mut().append_pair("token", &token);
        client.connect(url).await.unwrap()
    }

    /// A publisher announcing `cameras`, with its events stream open. The
    /// session ends with its control stream.
    async fn publisher(&self, cameras: &[&str]) -> (Session, ControlTx, EventsTx) {
        let session = self.connect("publish", Role::Publisher, "robot").await;
        let (send, recv) = session.open_bi().await.unwrap();
        let ctl = ControlTx::new(send);
        let mut rx = ControlRx::new(recv);
        ctl.send(&Control::Announce {
            room_id: "r1".into(),
            publisher_id: "pub".into(),
            cameras: cameras.iter().map(|id| camera(id)).collect(),
            build: Some(tq_proto::build_info!("tq-publisher")),
        })
        .await
        .unwrap();
        assert!(matches!(rx.recv().await.unwrap(), Some(Control::AnnounceOk { .. })));
        // Keep reading so the relay's pings never back up.
        tokio::spawn(async move { while let Ok(Some(_)) = rx.recv().await {} });
        let events = EventsTx::open(&session).await.unwrap();
        (session, ctl, events)
    }

    /// An operator or viewer: its control stream, session id and events.
    async fn participant(&self, role: Role, sub: &str) -> (Session, ControlTx, String, Watcher) {
        let session = self.connect("session", role, sub).await;
        let (send, recv) = session.open_bi().await.unwrap();
        let ctl = ControlTx::new(send);
        let mut rx = ControlRx::new(recv);
        ctl.send(&Control::Hello {
            build: tq_proto::build_info!("tq-test"),
        })
        .await
        .unwrap();
        let Some(Control::Welcome { session: id, .. }) = rx.recv().await.unwrap() else {
            panic!("no welcome");
        };
        tokio::spawn(async move { while let Ok(Some(_)) = rx.recv().await {} });
        let Incoming::Events(events) = Incoming::open(session.accept_uni().await.unwrap()).await.unwrap() else {
            panic!("expected the events stream first");
        };
        let mut w = Watcher {
            rx: events,
            seq: 0,
            state: None,
        };
        let RoomMessage::State { seq, state } = w.rx.recv().await.unwrap().unwrap() else {
            panic!("events stream must start with a state");
        };
        (w.seq, w.state) = (seq, Some(state));
        (session, ctl, id, w)
    }
}

fn camera(id: &str) -> CameraInfo {
    CameraInfo {
        id: id.into(),
        label: id.into(),
        codec: "h264".into(),
        width: 640,
        height: 480,
        fps: 30,
    }
}

/// Reads a participant's events stream, checking the sequence as it goes.
struct Watcher {
    rx: EventsRx,
    seq: u64,
    state: Option<RoomState>,
}

impl Watcher {
    /// The next event, skipping (and checking) states.
    async fn next(&mut self) -> Event {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(3), self.rx.recv())
                .await
                .expect("timed out waiting for an event")
                .unwrap()
                .unwrap();
            match msg {
                RoomMessage::State { seq, state } => {
                    assert!(seq >= self.seq, "state {seq} went back from {}", self.seq);
                    self.seq = seq;
                    self.state = Some(state);
                }
                RoomMessage::Event(RoomEvent { seq, event, .. }) => {
                    assert_eq!(seq, self.seq + 1, "gap before {event:?}");
                    self.seq = seq;
                    return event;
                }
                RoomMessage::Unknown => panic!("unknown message"),
            }
        }
    }

    /// Fails if any event arrives within a short while.
    async fn quiet(&mut self) {
        let r = tokio::time::timeout(Duration::from_millis(300), self.next()).await;
        assert!(r.is_err(), "unexpected event {:?}", r.unwrap());
    }
}

fn online(camera: &str) -> RoomMessage {
    robot(Event::TrackOnline { camera: camera.into() })
}

fn robot(event: Event) -> RoomMessage {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    RoomMessage::Event(RoomEvent {
        seq: SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
        ts_us: now_us(),
        event,
    })
}

#[tokio::test]
async fn room_events_end_to_end() {
    let h = Harness::start().await;
    let (_v, _vctl, viewer_id, mut v) = h.participant(Role::Viewer, "watcher").await;
    let state = v.state.clone().unwrap();
    assert_eq!(state.room_id, "r1");
    assert!(state.publisher.is_none());
    assert_eq!(state.participants.len(), 1);
    assert_eq!(state.participants[0].session, viewer_id);

    // The publisher arrives with two cameras; only its first edge for `a`
    // changes anything (`b` starts offline, the relay already has that).
    let (p1, _p1_ctl, mut p1_events) = h.publisher(&["a", "b"]).await;
    let e = v.next().await;
    assert!(
        matches!(&e, Event::PublisherConnected { publisher_id, .. } if publisher_id == "pub"),
        "{e:?}"
    );
    assert!(matches!(v.next().await, Event::TrackPublished { track } if track.camera.id == "a" && !track.online));
    assert!(matches!(v.next().await, Event::TrackPublished { track } if track.camera.id == "b"));
    p1_events.send(&online("a")).await.unwrap();
    p1_events
        .send(&robot(Event::TrackOffline {
            camera: "b".into(),
            reason: "source not connected".into(),
        }))
        .await
        .unwrap();
    p1_events
        .send(&robot(Event::DeadmanTripped {
            reason: "no operator".into(),
        }))
        .await
        .unwrap();
    // A relay-owned event from the publisher is refused.
    p1_events
        .send(&robot(Event::OperatorChanged {
            from: None,
            to: Some("x".into()),
            reason: OperatorChange::Forced,
        }))
        .await
        .unwrap();
    assert_eq!(v.next().await, Event::TrackOnline { camera: "a".into() });
    assert_eq!(
        v.next().await,
        Event::DeadmanTripped {
            reason: "no operator".into()
        }
    );

    // An operator joins and drives.
    let (o, octl, op_id, _ow) = h.participant(Role::Operator, "driver").await;
    assert!(
        matches!(v.next().await, Event::ParticipantJoined { participant } if participant.session == op_id && participant.role == Role::Operator && participant.build.component == "tq-test")
    );
    octl.send(&Control::Claim { force: false }).await.unwrap();
    assert_eq!(
        v.next().await,
        Event::OperatorChanged {
            from: None,
            to: Some(op_id.clone()),
            reason: OperatorChange::Claimed
        }
    );
    p1_events.send(&robot(Event::DeadmanCleared)).await.unwrap();
    assert_eq!(v.next().await, Event::DeadmanCleared);

    // The publisher drops: its online track goes offline, and it comes
    // back within the grace period with `b` gone and `c` new.
    p1.close(0, b"bye");
    assert!(matches!(v.next().await, Event::TrackOffline { camera, .. } if camera == "a"));
    assert!(matches!(
        v.next().await,
        Event::PublisherDisconnected {
            reason: PublisherGone::Disconnected,
            ..
        }
    ));
    let (_p2, _p2_ctl, _p2_events) = h.publisher(&["a", "c"]).await;
    assert!(matches!(v.next().await, Event::PublisherConnected { .. }));
    assert!(matches!(v.next().await, Event::TrackUnpublished { camera, .. } if camera == "b"));
    assert!(matches!(v.next().await, Event::TrackPublished { track } if track.camera.id == "c"));

    // A second publisher session takes over: no track changes.
    let (p3, _p3_ctl, _p3_events) = h.publisher(&["a", "c"]).await;
    assert!(matches!(
        v.next().await,
        Event::PublisherDisconnected {
            reason: PublisherGone::Replaced,
            ..
        }
    ));
    assert!(matches!(v.next().await, Event::PublisherConnected { .. }));
    v.quiet().await;

    // It drops for good: after the grace period its tracks are unpublished.
    p3.close(0, b"bye");
    assert!(matches!(v.next().await, Event::PublisherDisconnected { .. }));
    assert!(matches!(v.next().await, Event::TrackUnpublished { camera, .. } if camera == "a"));
    assert!(matches!(v.next().await, Event::TrackUnpublished { camera, .. } if camera == "c"));

    // The operator leaves while holding the claim.
    o.close(0, b"bye");
    assert_eq!(
        v.next().await,
        Event::OperatorChanged {
            from: Some(op_id.clone()),
            to: None,
            reason: OperatorChange::Left
        }
    );
    assert!(matches!(v.next().await, Event::ParticipantLeft { participant } if participant.session == op_id));

    // The viewer's state catches up with the relay's.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = tokio::time::timeout(Duration::from_millis(200), v.next()).await;
    let room = h.relay.rooms.lock().unwrap()["r1"].clone();
    assert_eq!(v.state.as_ref().unwrap(), &room.state());
    let s = room.state();
    assert!(s.publisher.is_none() && s.tracks.is_empty() && s.operator.is_none());
    assert_eq!(s.participants.len(), 1);
}

#[tokio::test]
async fn hello_is_required() {
    let h = Harness::start().await;
    let session = h.connect("session", Role::Viewer, "rude").await;
    let (send, recv) = session.open_bi().await.unwrap();
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    ctl.send(&Control::Claim { force: false }).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap();
    assert!(
        !matches!(r, Ok(Some(Control::Welcome { .. }))),
        "welcomed without a hello: {r:?}"
    );
}

#[tokio::test]
async fn rooms_api_needs_a_token_for_the_room() {
    let h = Harness::start().await;
    let (_v, _vctl, _id, _w) = h.participant(Role::Viewer, "watcher").await;
    let token = |room: &str| {
        sign_token(
            SECRET.as_bytes(),
            &Claims {
                sub: "ops".into(),
                role: Role::Viewer,
                room: room.into(),
                exp: now_us() / 1_000_000 + 60,
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
