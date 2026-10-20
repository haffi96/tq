//! The robot publisher's session.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use anyhow::Context;
use tokio::task::JoinSet;
use tq_proto::{clock_offset, now_us, Claims, Compat, Control, MediaFrame, RoomEvent, RoomMessage};
use tq_transport::{
    control::{ControlRx, ControlTx},
    events::{EventsRx, Incoming},
    quic::quic_stats,
    track::{Push, Track},
    Session,
};

use crate::{
    room::{PublisherLink, Room},
    Relay,
};

/// Clock pings to the publisher; the offset comes from the best of the last
/// `CLOCK_WINDOW` (10 s, spanning a Starlink handover cycle's quiet part).
const PING_INTERVAL: Duration = Duration::from_millis(500);
const CLOCK_WINDOW: usize = 20;

pub async fn run(relay: Arc<Relay>, room: Arc<Room>, session: Session, claims: Claims) -> anyhow::Result<()> {
    let conn_id = relay.next_conn.fetch_add(1, Relaxed);
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), session.accept_bi())
        .await
        .context("no control stream")??;
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    let Some(Control::Announce {
        room_id,
        publisher_id,
        cameras,
        build,
    }) = rx.recv().await?
    else {
        anyhow::bail!("expected announce");
    };
    let version = build
        .as_ref()
        .map_or_else(|| "unknown (protocol 1.0)".to_string(), |b| b.long());
    match Compat::of_build(build.as_ref()) {
        Compat::Same => {}
        Compat::MinorDiffers => {
            tracing::warn!(room = %room.id, %publisher_id, "{}", tq_proto::mismatch("publisher", build.as_ref(), "tq-relay"))
        }
        Compat::Incompatible => {
            let reason = tq_proto::mismatch("publisher", build.as_ref(), "tq-relay");
            let _ = ctl
                .send(&Control::Error {
                    message: format!("unsupported protocol: {reason}"),
                })
                .await;
            session.close(tq_proto::code::UNSUPPORTED_VERSION, reason.as_bytes());
            anyhow::bail!("refused publisher {publisher_id}: {reason}");
        }
    }
    if room_id != room.id {
        ctl.send(&Control::Error {
            message: format!("announce for {room_id} on /v1/publish/{}", room.id),
        })
        .await?;
        anyhow::bail!("room mismatch");
    }
    tracing::info!(room = %room.id, %publisher_id, sub = %claims.sub, cameras = cameras.len(), %version, "publisher announced");

    let old = room.publisher_connected(PublisherLink {
        conn_id,
        publisher_id: publisher_id.clone(),
        build,
        session: session.clone(),
        ctl: ctl.clone(),
        cameras: cameras.clone(),
        camera_status: Vec::new(),
        teleop: Default::default(),
        rtt_ms: None,
        clock: tq_proto::ClockFilter::new(CLOCK_WINDOW),
        quic: None,
    });
    if let Some(old) = old {
        tracing::warn!(room = %room.id, "replacing previous publisher session");
        old.session
            .close(tq_proto::code::REPLACED, b"replaced by a new publisher session");
    }
    ctl.send(&Control::AnnounceOk {
        relay_us: now_us(),
        relay: Some(relay.build.clone()),
    })
    .await?;
    room.notify_operator();

    let mut tasks = JoinSet::new();
    tasks.spawn(ingest(
        session.clone(),
        room.clone(),
        conn_id,
        cameras.iter().map(|c| c.id.clone()).collect(),
    ));
    tasks.spawn(control_loop(rx, room.clone(), conn_id));
    tasks.spawn(ping_loop(ctl.clone(), room.clone(), conn_id, session.clone()));
    tasks.spawn(datagrams(session.clone(), room.clone()));

    let result = tokio::select! {
        e = session.closed() => Err(anyhow::Error::from(e)),
        Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
    };
    tasks.abort_all();

    room.publisher_disconnected(conn_id);
    tracing::info!(room = %room.id, %publisher_id, "publisher gone");
    result
}

/// Accepts the publisher's unidirectional streams: one per GOP, pushed
/// into the camera's track, and its events stream.
async fn ingest(session: Session, room: Arc<Room>, conn_id: u64, camera_ids: Vec<String>) -> anyhow::Result<()> {
    let tracks: Vec<Arc<Track>> = {
        let inner = room.inner.lock().unwrap();
        camera_ids.iter().map(|id| inner.tracks[id].clone()).collect()
    };
    // Newest group started per camera on this connection. A group stream
    // whose first bytes arrive after a newer group's (loss reordering) is
    // ignored rather than allowed to restart the track on stale video.
    let newest: Arc<Vec<std::sync::atomic::AtomicU64>> =
        Arc::new(tracks.iter().map(|_| std::sync::atomic::AtomicU64::new(0)).collect());
    loop {
        let stream = session.accept_uni().await?;
        let tracks = tracks.clone();
        let newest = newest.clone();
        let room = room.clone();
        tokio::spawn(async move {
            let mut reader = match Incoming::open(stream).await {
                Ok(Incoming::Group(r)) => r,
                Ok(Incoming::Events(rx)) => {
                    if let Err(e) = robot_events(rx, &room, conn_id).await {
                        tracing::debug!(room = %room.id, error = format!("{e:#}"), "publisher events stream ended");
                    }
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "bad stream");
                    return;
                }
            };
            let camera = reader.header.camera;
            let Some(track) = tracks.get(camera as usize) else {
                tracing::warn!(camera, "group for unknown camera index");
                return;
            };
            let codec: Arc<str> = reader.header.codec.as_str().into();
            let group = reader.header.group;
            if newest[camera as usize].fetch_max(group, Relaxed) > group {
                reader.stop();
                return;
            }
            loop {
                match reader.next().await {
                    Ok(Some((mut header, payload))) => {
                        header.relay_us = now_us();
                        let frame = MediaFrame {
                            camera,
                            group,
                            codec: codec.clone(),
                            header,
                            payload,
                        };
                        if track.push(frame, camera) == Push::Dropped {
                            // Superseded by a newer group; stop reading this one.
                            reader.stop();
                            return;
                        }
                    }
                    Ok(None) => return,
                    // Reset by the publisher (late or superseded group).
                    Err(_) => return,
                }
            }
        });
    }
}

/// The publisher's events: track online/offline and the dead-man.
async fn robot_events(mut rx: EventsRx, room: &Room, conn_id: u64) -> anyhow::Result<()> {
    let mut last = 0u64;
    while let Some(msg) = rx.recv().await? {
        let RoomMessage::Event(RoomEvent { seq, ts_us, event }) = msg else {
            continue;
        };
        if seq != last + 1 {
            // Harmless: the publisher restates everything after a gap of
            // its own, and unchanged values are dropped.
            tracing::debug!(room = %room.id, expected = last + 1, seq, "gap in publisher events");
        }
        last = seq;
        if !event.from_robot() {
            tracing::warn!(room = %room.id, ?event, "publisher sent an event it does not own");
            continue;
        }
        room.robot_event(conn_id, event, ts_us);
    }
    Ok(())
}

async fn control_loop(mut rx: ControlRx, room: Arc<Room>, conn_id: u64) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::PublisherStatus { cameras, teleop } => {
                let mut inner = room.inner.lock().unwrap();
                if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
                    p.camera_status = cameras;
                    p.teleop = teleop;
                }
                drop(inner);
                room.bump();
            }
            Control::Pong { t0_us, t1_us, .. } => {
                let (offset, rtt) = clock_offset(t0_us, t1_us, now_us());
                let mut inner = room.inner.lock().unwrap();
                if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
                    p.rtt_ms = Some(rtt as f64 / 1000.0);
                    p.clock.add(offset, rtt);
                }
            }
            Control::Ping { id, t0_us } => {
                let ctl = room.inner.lock().unwrap().publisher.as_ref().map(|p| p.ctl.clone());
                if let Some(ctl) = ctl {
                    ctl.send(&Control::Pong {
                        id,
                        t0_us,
                        t1_us: now_us(),
                    })
                    .await?;
                }
            }
            other => tracing::debug!(?other, "ignored publisher message"),
        }
    }
    anyhow::bail!("publisher closed the control stream")
}

async fn ping_loop(ctl: ControlTx, room: Arc<Room>, conn_id: u64, session: Session) -> anyhow::Result<()> {
    let mut id = 0;
    let mut t = tokio::time::interval(PING_INTERVAL);
    loop {
        t.tick().await;
        id += 1;
        ctl.send(&Control::Ping { id, t0_us: now_us() }).await?;
        let mut inner = room.inner.lock().unwrap();
        if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
            p.quic = Some(quic_stats(&session));
        }
    }
}

/// Publisher -> operator datagrams (acks, heartbeat echoes) go to the
/// current claim holder only.
async fn datagrams(session: Session, room: Arc<Room>) -> anyhow::Result<()> {
    loop {
        let data = session.read_datagram().await?;
        let target = {
            let inner = room.inner.lock().unwrap();
            inner
                .operator
                .as_ref()
                .and_then(|op| inner.clients.get(op))
                .map(|c| c.session.clone())
        };
        if let Some(target) = target {
            let _ = target.send_datagram(data);
        }
    }
}
