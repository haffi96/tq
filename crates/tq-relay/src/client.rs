//! Operator and viewer sessions.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use bytes::Bytes;
use tokio::{
    sync::{broadcast, broadcast::error::RecvError},
    task::JoinSet,
};
use tq_proto::{now_us, Claims, Compat, Control, ParticipantInfo, Role};
use tq_transport::{
    control::{ControlRx, ControlTx},
    events::EventsTx,
    group::{GroupSender, Outcome, SendPolicy},
    quic::quic_stats,
    track::Track,
    Session,
};

use crate::{
    room::{ClientLink, Room},
    Relay,
};

pub async fn run(relay: Arc<Relay>, room: Arc<Room>, session: Session, claims: Claims) -> anyhow::Result<()> {
    let id = random_id();
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), session.accept_bi())
        .await
        .context("no control stream")??;
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    // The client's build comes first; it joins the room with it.
    let build = match tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .context("no hello")??
    {
        Some(Control::Hello { build }) => build,
        other => anyhow::bail!("expected hello, got {other:?}"),
    };
    let name = build.component.clone();
    match Compat::of_build(Some(&build)) {
        Compat::Same => {}
        Compat::MinorDiffers => {
            tracing::warn!(room = %room.id, session = %id, "{}", tq_proto::mismatch(&name, Some(&build), "tq-relay"))
        }
        Compat::Incompatible => {
            let reason = tq_proto::mismatch(&name, Some(&build), "tq-relay");
            session.close(tq_proto::code::UNSUPPORTED_VERSION, reason.as_bytes());
            anyhow::bail!("refused {name}: {reason}");
        }
    }
    tracing::info!(room = %room.id, session = %id, role = %claims.role, sub = %claims.sub, client = %build, "client joined");
    ctl.send(&Control::Welcome {
        session: id.clone(),
        role: claims.role,
        relay: Some(relay.build.clone()),
    })
    .await?;
    room.join(ClientLink {
        info: ParticipantInfo {
            session: id.clone(),
            role: claims.role,
            sub: claims.sub.clone(),
            build,
            joined_us: now_us(),
        },
        session: session.clone(),
    });

    let policy = match claims.role {
        Role::Operator => relay.operator_policy,
        _ => relay.viewer_policy,
    };
    let mut tasks = JoinSet::new();
    tasks.spawn(events(session.clone(), room.clone()));
    tasks.spawn(control_loop(rx, ctl.clone(), room.clone(), id.clone(), claims.role));
    tasks.spawn(link_stats(ctl.clone(), session.clone()));
    tasks.spawn(video(session.clone(), room.clone(), policy));
    if claims.role == Role::Operator {
        tasks.spawn(datagrams(session.clone(), room.clone(), id.clone()));
    }

    let result = tokio::select! {
        e = session.closed() => Err(anyhow::Error::from(e)),
        Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
    };
    tasks.abort_all();
    room.leave(&id);
    tracing::info!(room = %room.id, session = %id, "client left");
    result
}

fn random_id() -> String {
    let n: u64 = rand::random();
    format!("{n:016x}")
}

async fn control_loop(
    mut rx: ControlRx,
    ctl: ControlTx,
    room: Arc<Room>,
    id: String,
    role: Role,
) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::Ping { id: pid, t0_us } => {
                ctl.send(&Control::Pong {
                    id: pid,
                    t0_us,
                    t1_us: now_us(),
                })
                .await?
            }
            Control::Claim { force } => {
                let reply = match role {
                    Role::Operator => match room.claim(&id, force) {
                        Ok(holder) => Control::ClaimResult {
                            ok: true,
                            holder: Some(holder),
                            reason: None,
                        },
                        Err(holder) => Control::ClaimResult {
                            ok: false,
                            holder: Some(holder),
                            reason: Some("another operator holds the claim; force to take over".into()),
                        },
                    },
                    _ => Control::ClaimResult {
                        ok: false,
                        holder: None,
                        reason: Some("viewers cannot drive".into()),
                    },
                };
                ctl.send(&reply).await?;
            }
            Control::Release => {
                let released = room.release(&id);
                ctl.send(&Control::ClaimResult {
                    ok: released,
                    holder: None,
                    reason: None,
                })
                .await?;
            }
            Control::KeyframeRequest { camera, reason } => room.request_keyframe(&camera, &reason),
            other => tracing::debug!(?other, "ignored client message"),
        }
    }
    anyhow::bail!("client closed the control stream")
}

/// The session's events stream.
async fn events(session: Session, room: Arc<Room>) -> anyhow::Result<()> {
    let mut tx = EventsTx::open(&session).await?;
    pump_events(&room, &mut tx).await
}

/// Where [`pump_events`] writes encoded room messages.
pub trait EventsOut {
    async fn send(&mut self, bytes: &Bytes) -> anyhow::Result<()>;
}

impl EventsOut for EventsTx {
    async fn send(&mut self, bytes: &Bytes) -> anyhow::Result<()> {
        self.send_encoded(bytes).await
    }
}

/// Writes the room's state, then every event in order, and a new state
/// whenever the room changes (at most every 100 ms) once the events up to
/// it have gone out. A session that falls more than the broadcast buffer
/// behind (a slow link backs up `out`) gets a state instead of the events
/// it missed. Sequence numbers never go back.
pub async fn pump_events(room: &Room, out: &mut impl EventsOut) -> anyhow::Result<()> {
    let mut changed = room.version.subscribe();
    let mut events = room.subscribe();
    changed.borrow_and_update();
    let state = room.snapshot();
    out.send(&state.bytes).await?;
    // Sequence number the client is up to.
    let mut sent = state.seq;
    loop {
        tokio::select! {
            r = events.recv() => match r {
                Ok(e) => {
                    if e.seq > sent {
                        out.send(&e.bytes).await?;
                        sent = e.seq;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!(room = %room.id, missed = n, "events lagged, resyncing");
                    let state = room.snapshot();
                    if state.seq >= sent {
                        out.send(&state.bytes).await?;
                        sent = state.seq;
                    }
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            r = changed.changed() => {
                r?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                changed.borrow_and_update();
                // Events first: an event emitted before the line above is
                // in the channel by now.
                let mut lagged = false;
                loop {
                    match events.try_recv() {
                        Ok(e) if e.seq > sent => {
                            out.send(&e.bytes).await?;
                            sent = e.seq;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            tracing::debug!(room = %room.id, missed = n, "events lagged, resyncing");
                            lagged = true;
                            break;
                        }
                    }
                }
                let state = room.snapshot();
                // A state ahead of `sent` would swallow events still on
                // their way; they bumped the version after the line above,
                // so the next round sends it. After a lag it replaces the
                // missed events.
                if state.seq == sent || (lagged && state.seq > sent) {
                    out.send(&state.bytes).await?;
                    sent = state.seq;
                }
            }
        }
    }
}

/// QUIC stats of this connection, every second.
async fn link_stats(ctl: ControlTx, session: Session) -> anyhow::Result<()> {
    let mut stats = tokio::time::interval(Duration::from_secs(1));
    loop {
        stats.tick().await;
        ctl.send(&Control::LinkStats {
            quic: quic_stats(&session),
        })
        .await?;
    }
}

/// Runs one sender per camera; restarts them when the camera list changes.
async fn video(session: Session, room: Arc<Room>, policy: SendPolicy) -> anyhow::Result<()> {
    let mut cameras = room.cameras_version.subscribe();
    loop {
        cameras.borrow_and_update();
        let mut senders = JoinSet::new();
        for (index, info, track) in room.tracks() {
            senders.spawn(send_camera(
                session.clone(),
                room.clone(),
                index,
                info.id,
                track,
                policy,
            ));
        }
        tokio::select! {
            r = cameras.changed() => r?,
            Some(r) = senders.join_next() => {
                r??;
            }
        }
        senders.abort_all();
    }
}

async fn send_camera(
    session: Session,
    room: Arc<Room>,
    index: u16,
    camera: String,
    track: Arc<Track>,
    policy: SendPolicy,
) -> anyhow::Result<()> {
    let mut sender = GroupSender::new(session, policy);
    let (cache, mut rx) = track.subscribe();
    let send = async |sender: &mut GroupSender, f: &tq_proto::MediaFrame| -> anyhow::Result<()> {
        // Frames already carry the relay's camera index, but the camera
        // list may have been reordered since; trust the current index.
        let mut f = f.clone();
        f.camera = index;
        if sender.send(&f, f.header.relay_us).await? == Outcome::Late {
            room.request_keyframe(&camera, "relay -> client late");
        }
        Ok(())
    };
    for f in &cache {
        send(&mut sender, f).await?;
    }
    loop {
        match rx.recv().await {
            Ok(f) => send(&mut sender, &f).await?,
            Err(RecvError::Lagged(_)) => {
                sender.abandon();
                room.request_keyframe(&camera, "client lagged");
            }
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

/// Operator -> publisher datagrams, forwarded only from the claim holder.
async fn datagrams(session: Session, room: Arc<Room>, id: String) -> anyhow::Result<()> {
    loop {
        let data = session.read_datagram().await?;
        let target = {
            let inner = room.inner.lock().unwrap();
            if inner.operator.as_deref() != Some(id.as_str()) {
                continue;
            }
            inner.publisher.as_ref().map(|p| p.session.clone())
        };
        if let Some(target) = target {
            let _ = target.send_datagram(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, watch};
    use tq_proto::{Event, RoomMessage};

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
        let room = Room::new("r".into(), Duration::from_secs(30));
        room.claim("a", false).unwrap();
        let (open_tx, open) = watch::channel(false);
        let (out, mut rx) = mpsc::unbounded_channel();
        let pump = {
            let room = room.clone();
            tokio::spawn(async move { pump_events(&room, &mut Gated { open, sent: 0, out }).await })
        };
        // The first state, as of the claim.
        let first = rx.recv().await.unwrap();
        assert!(matches!(first, RoomMessage::State { seq: 1, .. }));

        // One event, then far more than the buffer while the session is
        // stuck on it.
        room.release("a");
        tokio::time::sleep(Duration::from_millis(20)).await;
        for _ in 0..200 {
            room.claim("a", false).unwrap();
            room.release("a");
        }
        let last = 402;
        open_tx.send(true).unwrap();

        let mut got = vec![first];
        while got.last().map(seq) != Some(last) {
            got.push(
                tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        pump.abort();

        // Sequence numbers never go back, events follow on from the last
        // state or event without gaps, and the lag was covered by a state.
        let mut at = 0;
        let mut resynced = false;
        for m in &got {
            match m {
                RoomMessage::State { seq, state } => {
                    assert!(*seq >= at, "state {seq} after {at}");
                    resynced |= *seq > 2;
                    // Released after every claim: the state agrees.
                    if *seq % 2 == 0 {
                        assert_eq!(state.operator, None);
                    }
                    at = *seq;
                }
                RoomMessage::Event(e) => {
                    assert_eq!(e.seq, at + 1, "gap before event {}", e.seq);
                    assert!(matches!(e.event, Event::OperatorChanged { .. }));
                    at = e.seq;
                }
                RoomMessage::Unknown => unreachable!(),
            }
        }
        assert!(resynced, "no resync state: {} messages", got.len());
        assert!(
            got.len() < 100,
            "lagged session replayed everything: {} messages",
            got.len()
        );
    }
}
