//! The robot publisher's session.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use anyhow::Context;
use tokio::task::JoinSet;
use tq_proto::{clock_offset, now_us, Claims, Compat, Control, MediaFrame};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::GroupReader,
    quic::quic_stats,
    track::{Push, Track},
    Session,
};

use crate::{
    av::{Av, PublisherLink},
    Relay,
};

/// Clock pings to the publisher; the offset comes from the best of the last
/// `CLOCK_WINDOW` (10 s, spanning a Starlink handover cycle's quiet part).
const PING_INTERVAL: Duration = Duration::from_millis(500);
const CLOCK_WINDOW: usize = 20;

pub async fn run(relay: Arc<Relay>, av: Arc<Av>, session: Session, claims: Claims) -> anyhow::Result<()> {
    let conn_id = relay.next_conn.fetch_add(1, Relaxed);
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), session.accept_bi())
        .await
        .context("no control stream")??;
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    let Some(Control::Announce {
        av_id,
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
            tracing::warn!(av = %av.id, %publisher_id, "{}", tq_proto::mismatch("publisher", build.as_ref(), "tq-relay"))
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
    if av_id != av.id {
        ctl.send(&Control::Error {
            message: format!("announce for {av_id} on /v1/publish/{}", av.id),
        })
        .await?;
        anyhow::bail!("av mismatch");
    }
    tracing::info!(av = %av.id, %publisher_id, sub = %claims.sub, cameras = cameras.len(), %version, "publisher announced");

    let old = {
        let mut inner = av.inner.lock().unwrap();
        let cameras_changed = inner.cameras != cameras;
        for c in &cameras {
            inner.tracks.entry(c.id.clone()).or_insert_with(Track::new);
        }
        inner.cameras = cameras.clone();
        let old = inner.publisher.replace(PublisherLink {
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
        if cameras_changed {
            av.cameras_version.send_modify(|v| *v += 1);
        }
        old
    };
    if let Some(old) = old {
        tracing::warn!(av = %av.id, "replacing previous publisher session");
        old.session
            .close(tq_proto::code::REPLACED, b"replaced by a new publisher session");
    }
    ctl.send(&Control::AnnounceOk {
        relay_us: now_us(),
        relay: Some(relay.build.clone()),
    })
    .await?;
    av.notify_operator();
    av.bump();

    let mut tasks = JoinSet::new();
    tasks.spawn(ingest(
        session.clone(),
        av.clone(),
        cameras.iter().map(|c| c.id.clone()).collect(),
    ));
    tasks.spawn(control_loop(rx, av.clone(), conn_id));
    tasks.spawn(ping_loop(ctl.clone(), av.clone(), conn_id, session.clone()));
    tasks.spawn(datagrams(session.clone(), av.clone()));

    let result = tokio::select! {
        e = session.closed() => Err(anyhow::Error::from(e)),
        Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
    };
    tasks.abort_all();

    {
        let mut inner = av.inner.lock().unwrap();
        if inner.publisher.as_ref().is_some_and(|p| p.conn_id == conn_id) {
            inner.publisher = None;
            for t in inner.tracks.values() {
                t.reset();
            }
        }
    }
    av.bump();
    tracing::info!(av = %av.id, %publisher_id, "publisher gone");
    result
}

/// Accepts one unidirectional stream per GOP and pushes its frames into the
/// camera's track.
async fn ingest(session: Session, av: Arc<Av>, camera_ids: Vec<String>) -> anyhow::Result<()> {
    let tracks: Vec<Arc<Track>> = {
        let inner = av.inner.lock().unwrap();
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
        tokio::spawn(async move {
            let mut reader = match GroupReader::open(stream).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(error = %e, "bad group stream");
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

async fn control_loop(mut rx: ControlRx, av: Arc<Av>, conn_id: u64) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::PublisherStatus { cameras, teleop } => {
                let mut inner = av.inner.lock().unwrap();
                if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
                    p.camera_status = cameras;
                    p.teleop = teleop;
                }
                drop(inner);
                av.bump();
            }
            Control::Pong { t0_us, t1_us, .. } => {
                let (offset, rtt) = clock_offset(t0_us, t1_us, now_us());
                let mut inner = av.inner.lock().unwrap();
                if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
                    p.rtt_ms = Some(rtt as f64 / 1000.0);
                    p.clock.add(offset, rtt);
                }
            }
            Control::Ping { id, t0_us } => {
                let ctl = av.inner.lock().unwrap().publisher.as_ref().map(|p| p.ctl.clone());
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

async fn ping_loop(ctl: ControlTx, av: Arc<Av>, conn_id: u64, session: Session) -> anyhow::Result<()> {
    let mut id = 0;
    let mut t = tokio::time::interval(PING_INTERVAL);
    loop {
        t.tick().await;
        id += 1;
        ctl.send(&Control::Ping { id, t0_us: now_us() }).await?;
        let mut inner = av.inner.lock().unwrap();
        if let Some(p) = inner.publisher.as_mut().filter(|p| p.conn_id == conn_id) {
            p.quic = Some(quic_stats(&session));
        }
    }
}

/// Publisher -> operator datagrams (acks, heartbeat echoes) go to the
/// current claim holder only.
async fn datagrams(session: Session, av: Arc<Av>) -> anyhow::Result<()> {
    loop {
        let data = session.read_datagram().await?;
        let target = {
            let inner = av.inner.lock().unwrap();
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
