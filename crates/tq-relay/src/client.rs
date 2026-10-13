//! Browser sessions: operators and viewers.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use tokio::{sync::broadcast::error::RecvError, task::JoinSet};
use tq_proto::{now_us, Claims, Compat, Control, Role};
use tq_transport::{
    control::{ControlRx, ControlTx},
    group::{GroupSender, Outcome, SendPolicy},
    quic::quic_stats,
    track::Track,
    Session,
};

use crate::{
    av::{Av, ClientLink},
    Relay,
};

pub async fn run(relay: Arc<Relay>, av: Arc<Av>, session: Session, claims: Claims) -> anyhow::Result<()> {
    let id = random_id();
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), session.accept_bi())
        .await
        .context("no control stream")??;
    let ctl = ControlTx::new(send);
    let rx = ControlRx::new(recv);
    {
        let mut inner = av.inner.lock().unwrap();
        inner.clients.insert(
            id.clone(),
            ClientLink {
                role: claims.role,
                sub: claims.sub.clone(),
                build: None,
                session: session.clone(),
                ctl: ctl.clone(),
            },
        );
    }
    av.bump();
    tracing::info!(av = %av.id, session = %id, role = %claims.role, sub = %claims.sub, "client joined");
    ctl.send(&Control::Welcome {
        session: id.clone(),
        role: claims.role,
        state: av.state(),
        relay: Some(relay.build.clone()),
    })
    .await?;

    let policy = match claims.role {
        Role::Operator => relay.operator_policy,
        _ => relay.viewer_policy,
    };
    let mut tasks = JoinSet::new();
    tasks.spawn(control_loop(
        rx,
        ctl.clone(),
        session.clone(),
        av.clone(),
        id.clone(),
        claims.role,
    ));
    tasks.spawn(state_loop(ctl.clone(), av.clone(), session.clone()));
    tasks.spawn(video(session.clone(), av.clone(), policy));
    if claims.role == Role::Operator {
        tasks.spawn(datagrams(session.clone(), av.clone(), id.clone()));
    }

    let result = tokio::select! {
        e = session.closed() => Err(anyhow::Error::from(e)),
        Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
    };
    tasks.abort_all();

    let was_operator = {
        let mut inner = av.inner.lock().unwrap();
        inner.clients.remove(&id);
        if inner.operator.as_deref() == Some(id.as_str()) {
            inner.operator = None;
            true
        } else {
            false
        }
    };
    if was_operator {
        tracing::info!(av = %av.id, session = %id, "operator left, claim released");
        av.notify_operator();
    }
    av.bump();
    tracing::info!(av = %av.id, session = %id, "client left");
    result
}

fn random_id() -> String {
    let n: u64 = rand::random();
    format!("{n:016x}")
}

async fn control_loop(
    mut rx: ControlRx,
    ctl: ControlTx,
    session: Session,
    av: Arc<Av>,
    id: String,
    role: Role,
) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::Hello { build } => {
                let name = build.component.clone();
                match Compat::of_build(Some(&build)) {
                    Compat::Same => tracing::info!(av = %av.id, session = %id, client = %build, "client hello"),
                    Compat::MinorDiffers => {
                        tracing::warn!(av = %av.id, session = %id, "{}", tq_proto::mismatch(&name, Some(&build), "tq-relay"))
                    }
                    Compat::Incompatible => {
                        let reason = tq_proto::mismatch(&name, Some(&build), "tq-relay");
                        session.close(tq_proto::code::UNSUPPORTED_VERSION, reason.as_bytes());
                        anyhow::bail!("refused {name}: {reason}");
                    }
                }
                if let Some(link) = av.inner.lock().unwrap().clients.get_mut(&id) {
                    link.build = Some(build);
                }
            }
            Control::Ping { id: pid, t0_us } => {
                ctl.send(&Control::Pong {
                    id: pid,
                    t0_us,
                    t1_us: now_us(),
                })
                .await?
            }
            Control::Claim { force } => {
                let reply = claim(&av, &id, role, force);
                ctl.send(&reply).await?;
            }
            Control::Release => {
                let released = {
                    let mut inner = av.inner.lock().unwrap();
                    if inner.operator.as_deref() == Some(id.as_str()) {
                        inner.operator = None;
                        true
                    } else {
                        false
                    }
                };
                if released {
                    tracing::info!(av = %av.id, session = %id, "operator released claim");
                    av.notify_operator();
                    av.bump();
                }
                ctl.send(&Control::ClaimResult {
                    ok: released,
                    holder: None,
                    reason: None,
                })
                .await?;
            }
            Control::KeyframeRequest { camera, reason } => av.request_keyframe(&camera, &reason),
            other => tracing::debug!(?other, "ignored client message"),
        }
    }
    anyhow::bail!("client closed the control stream")
}

fn claim(av: &Av, id: &str, role: Role, force: bool) -> Control {
    if role != Role::Operator {
        return Control::ClaimResult {
            ok: false,
            holder: None,
            reason: Some("viewers cannot drive".into()),
        };
    }
    let (ok, holder, previous) = {
        let mut inner = av.inner.lock().unwrap();
        match inner.operator.clone() {
            Some(h) if h == id => (true, Some(h), None),
            Some(h) if !force => (false, Some(h), None),
            prev => {
                inner.operator = Some(id.to_string());
                (true, Some(id.to_string()), prev)
            }
        }
    };
    if ok {
        tracing::info!(av = %av.id, session = %id, taken_from = ?previous, "operator claim");
        av.notify_operator();
        av.bump();
        Control::ClaimResult {
            ok,
            holder,
            reason: None,
        }
    } else {
        Control::ClaimResult {
            ok,
            holder,
            reason: Some("another operator holds the claim; force to take over".into()),
        }
    }
}

/// Sends the relay state whenever it changes (at most every 100 ms), and
/// connection stats every second.
async fn state_loop(ctl: ControlTx, av: Arc<Av>, session: Session) -> anyhow::Result<()> {
    let mut changed = av.version.subscribe();
    let mut stats = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            r = changed.changed() => {
                r?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                changed.borrow_and_update();
                ctl.send(&Control::State { state: av.state() }).await?;
            }
            _ = stats.tick() => {
                ctl.send(&Control::LinkStats { quic: quic_stats(&session) }).await?;
            }
        }
    }
}

/// Runs one sender per camera; restarts them when the camera list changes.
async fn video(session: Session, av: Arc<Av>, policy: SendPolicy) -> anyhow::Result<()> {
    let mut cameras = av.cameras_version.subscribe();
    loop {
        cameras.borrow_and_update();
        let mut senders = JoinSet::new();
        for (index, info, track) in av.tracks() {
            senders.spawn(send_camera(session.clone(), av.clone(), index, info.id, track, policy));
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
    av: Arc<Av>,
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
            av.request_keyframe(&camera, "relay -> client late");
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
                av.request_keyframe(&camera, "client lagged");
            }
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

/// Operator -> publisher datagrams, forwarded only from the claim holder.
async fn datagrams(session: Session, av: Arc<Av>, id: String) -> anyhow::Result<()> {
    loop {
        let data = session.read_datagram().await?;
        let target = {
            let inner = av.inner.lock().unwrap();
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
