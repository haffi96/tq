//! One session: any client of the relay. What it may do comes from its
//! token's permissions, not from what kind of client it is. A robot
//! advertises a broadcast and receives channel messages; a console
//! subscribes, claims locks and sends on channels; one session may do all
//! of it.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering::Relaxed, Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use tokio::{
    sync::{broadcast, broadcast::error::RecvError, mpsc},
    task::{AbortHandle, JoinSet},
};
use tq_transport::{
    control::{ControlRx, ControlTx},
    events::{EventsRx, EventsTx, Incoming},
    group::{GroupSender, Outcome},
    quic::quic_stats,
    Session,
};
use tq_wire::{
    clock_offset, now_us, Claims, Compat, Control, Datagram, Delivery, MediaFrame, ParticipantInfo, Path, Policy,
    RoomEvent, RoomMessage, Subscription, TrackKind, Verb, FLAG_CAPTURE_FROM_SEI,
};

use crate::{
    room::{Outbox, RelayTrack, Resolved, Room, SessionLink},
    Relay,
};

/// Clock pings to every session; the offset comes from the best of the
/// last `CLOCK_WINDOW` (10 s, spanning a Starlink handover cycle's quiet
/// part).
const PING_INTERVAL: Duration = Duration::from_millis(500);
const CLOCK_WINDOW: usize = 20;

pub async fn run(relay: Arc<Relay>, room: Arc<Room>, session: Session, claims: Claims) -> anyhow::Result<()> {
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), session.accept_bi())
        .await
        .context("no control stream")??;
    let ctl = ControlTx::new(send);
    let mut rx = ControlRx::new(recv);
    // The session's build comes first; it joins the room with it.
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
            tracing::warn!(room = %room.id, "{}", tq_wire::mismatch(&name, Some(&build), "tq-relay"))
        }
        Compat::Incompatible => {
            let reason = tq_wire::mismatch(&name, Some(&build), "tq-relay");
            session.close(tq_wire::code::UNSUPPORTED_VERSION, reason.as_bytes());
            anyhow::bail!("refused {name}: {reason}");
        }
    }
    let sid = room.next_sid();
    let id = random_id();
    tracing::info!(room = %room.id, session = %id, sid, sub = %claims.sub, perms = ?claims.perms.0, client = %build, "session joined");
    ctl.send(&Control::Welcome {
        session: id.clone(),
        sid,
        perms: claims.perms.clone(),
        relay: relay.build.clone(),
    })
    .await?;

    let (out, out_rx) = mpsc::unbounded_channel();
    room.join(SessionLink {
        info: ParticipantInfo {
            session: id.clone(),
            sid,
            sub: claims.sub.clone(),
            perms: claims.perms.clone(),
            build: build.clone(),
            joined_us: now_us(),
        },
        session: session.clone(),
        out: out.clone(),
        clock: tq_wire::ClockFilter::new(CLOCK_WINDOW),
        rtt_ms: None,
        quic: None,
        owned: Vec::new(),
    });

    let ctx = Ctx {
        relay,
        room: room.clone(),
        session: session.clone(),
        sid,
        claims,
        build,
        out,
    };
    let mut tasks = JoinSet::new();
    tasks.spawn(outbox(ctl, out_rx));
    tasks.spawn(events(session.clone(), room.clone()));
    tasks.spawn(ping_loop(ctx.clone()));
    tasks.spawn(incoming(ctx.clone()));
    tasks.spawn(datagrams(ctx.clone()));
    tasks.spawn(control_loop(ctx, rx));

    let result = tokio::select! {
        e = session.closed() => Err(anyhow::Error::from(e)),
        Some(r) = tasks.join_next() => r.map_err(anyhow::Error::from).and_then(|r| r),
    };
    tasks.abort_all();
    room.leave(sid);
    tracing::info!(room = %room.id, session = %id, "session left");
    result
}

fn random_id() -> String {
    let n: u64 = rand::random();
    format!("{n:016x}")
}

#[derive(Clone)]
struct Ctx {
    relay: Arc<Relay>,
    room: Arc<Room>,
    session: Session,
    sid: u32,
    claims: Claims,
    build: tq_wire::BuildInfo,
    out: Outbox,
}

impl Ctx {
    fn send(&self, msg: Control) {
        let _ = self.out.send(msg);
    }
}

/// Writes queued control messages in order.
async fn outbox(ctl: ControlTx, mut rx: mpsc::UnboundedReceiver<Control>) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await {
        ctl.send(&msg).await?;
    }
    Ok(())
}

async fn control_loop(ctx: Ctx, mut rx: ControlRx) -> anyhow::Result<()> {
    let room = &ctx.room;
    let perms = &ctx.claims.perms;
    let mut subs: HashMap<Path, AbortHandle> = HashMap::new();
    let mut sub_tasks = JoinSet::new();
    while let Some(msg) = rx.recv().await? {
        match msg {
            Control::Advertise(ad) => {
                let name = ad.broadcast.clone();
                let refuse = |reason: String| Control::AdvertiseError {
                    broadcast: name.clone(),
                    reason,
                };
                if !perms.allows_pub(&name) {
                    ctx.send(refuse(format!("not allowed to publish '{name}'")));
                    continue;
                }
                if let Err(e) = ad.validate() {
                    ctx.send(refuse(e));
                    continue;
                }
                tracing::info!(room = %room.id, sid = ctx.sid, broadcast = %name, tracks = ad.tracks.len(), channels = ad.channels.len(), locks = ad.locks.len(), "advertised");
                let done = room.advertise(ctx.sid, ad, Some(ctx.build.clone()));
                if let Some(prev) = done.replaced {
                    replaced(room, prev, &name);
                }
            }
            Control::Unadvertise { broadcast } => {
                room.unadvertise(ctx.sid, &broadcast);
            }
            Control::TrackStats { broadcast, tracks } => room.owner_stats(ctx.sid, &broadcast, tracks),
            Control::Subscribe { tracks } => {
                for sub in tracks {
                    let path = sub.track.clone();
                    if !perms.allows(Verb::Sub, &path) {
                        ctx.send(Control::SubscribeResult {
                            track: path.clone(),
                            ok: false,
                            reason: Some(format!("not allowed to subscribe to '{path}'")),
                        });
                        continue;
                    }
                    if let Some(old) = subs.remove(&path) {
                        old.abort();
                    }
                    let h = sub_tasks.spawn(subscription(ctx.clone(), sub));
                    subs.insert(path.clone(), h);
                    room.subscribe(ctx.sid, &path);
                    ctx.send(Control::SubscribeResult {
                        track: path,
                        ok: true,
                        reason: None,
                    });
                }
            }
            Control::Unsubscribe { tracks } => {
                for path in tracks {
                    if let Some(h) = subs.remove(&path) {
                        h.abort();
                        room.unsubscribe(ctx.sid, &path);
                    }
                }
            }
            Control::KeyframeRequest { track, reason } => {
                if subs.contains_key(&track) {
                    room.request_keyframe(&track, &reason);
                }
            }
            Control::Claim { lock, force } => {
                let reply = if !perms.allows(Verb::Lock, &lock) && !perms.allows(Verb::Force, &lock) {
                    Control::ClaimResult {
                        lock,
                        ok: false,
                        holder: None,
                        reason: Some("not allowed to claim this lock".into()),
                    }
                } else {
                    match room.claim(ctx.sid, &lock, force) {
                        Ok(holder) => Control::ClaimResult {
                            lock,
                            ok: true,
                            holder: Some(holder),
                            reason: None,
                        },
                        Err((holder, reason)) => Control::ClaimResult {
                            lock,
                            ok: false,
                            holder,
                            reason: Some(reason),
                        },
                    }
                };
                ctx.send(reply);
            }
            Control::Release { lock } => {
                let ok = room.release(ctx.sid, &lock);
                ctx.send(Control::ClaimResult {
                    lock,
                    ok,
                    holder: None,
                    reason: None,
                });
            }
            Control::Ping { id, t0_us } => ctx.send(Control::Pong {
                id,
                t0_us,
                t1_us: now_us(),
            }),
            Control::Pong { t0_us, t1_us, .. } => {
                let (offset, rtt) = clock_offset(t0_us, t1_us, now_us());
                room.pong(ctx.sid, offset, rtt);
            }
            other => tracing::debug!(?other, "ignored session message"),
        }
        // Reap finished subscriptions; they only end on error.
        while let Some(r) = sub_tasks.try_join_next() {
            if let Ok(Err(e)) = r {
                tracing::debug!(error = format!("{e:#}"), "subscription ended");
            }
        }
    }
    anyhow::bail!("session closed the control stream")
}

/// `prev` lost `name` to another session: closed if that was all it
/// published, told otherwise.
fn replaced(room: &Room, prev: u32, name: &str) {
    let inner = room.inner.lock().unwrap();
    let Some(link) = inner.sessions.get(&prev) else {
        return;
    };
    tracing::warn!(room = %room.id, broadcast = %name, sid = prev, "broadcast taken over by a new session");
    if link.owned.iter().all(Option::is_none) {
        link.session
            .close(tq_wire::code::REPLACED, b"replaced by a new session");
    } else {
        let _ = link.out.send(Control::Error {
            message: format!("broadcast '{name}' was taken over by another session"),
        });
    }
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
    let mut events = room.subscribe_events();
    changed.borrow_and_update();
    let state = room.snapshot();
    out.send(&state.bytes).await?;
    // Sequence number the session is up to.
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

/// Clock pings and QUIC stats; the session's own link stats every second.
async fn ping_loop(ctx: Ctx) -> anyhow::Result<()> {
    let mut id = 0u64;
    let mut t = tokio::time::interval(PING_INTERVAL);
    loop {
        t.tick().await;
        id += 1;
        ctx.send(Control::Ping { id, t0_us: now_us() });
        let quic = quic_stats(&ctx.session);
        ctx.room.set_quic(ctx.sid, quic.clone());
        if id.is_multiple_of(2) {
            ctx.send(Control::LinkStats { quic });
        }
    }
}

// --- from the session: group streams and its events ----------------------

/// Accepts the session's unidirectional streams: groups of the tracks it
/// publishes, and its events stream.
async fn incoming(ctx: Ctx) -> anyhow::Result<()> {
    // Newest group started per (broadcast, track) on this connection. A
    // group stream whose first bytes arrive after a newer group's (loss
    // reordering) is ignored rather than allowed to restart the track on
    // stale data.
    let newest: Arc<Mutex<HashMap<(u16, u16), u64>>> = Default::default();
    loop {
        let stream = ctx.session.accept_uni().await?;
        let ctx = ctx.clone();
        let newest = newest.clone();
        tokio::spawn(async move {
            let mut reader = match Incoming::open(stream).await {
                Ok(Incoming::Group(r)) => r,
                Ok(Incoming::Events(rx)) => {
                    if let Err(e) = owner_events(rx, &ctx).await {
                        tracing::debug!(room = %ctx.room.id, error = format!("{e:#}"), "session events stream ended");
                    }
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "bad stream");
                    return;
                }
            };
            let h = reader.header.clone();
            let Some((room_index, info, track)) = ctx.room.owner_track(ctx.sid, h.broadcast, h.track) else {
                tracing::debug!(broadcast = h.broadcast, track = h.track, "group for an unknown track");
                reader.stop();
                return;
            };
            if info.is_datagram() {
                reader.stop();
                return;
            }
            {
                let mut newest = newest.lock().unwrap();
                let n = newest.entry((h.broadcast, h.track)).or_insert(0);
                if *n > h.group {
                    reader.stop();
                    return;
                }
                *n = h.group;
            }
            let codec: Arc<str> = h.codec.as_str().into();
            let c = &track.counters;
            if info.kind == TrackKind::Video {
                let mut cs = c.codec_string.lock().unwrap();
                if cs.as_deref() != Some(&h.codec) {
                    *cs = Some(h.codec.clone());
                }
            }
            loop {
                match reader.next().await {
                    Ok(Some((mut header, payload))) => {
                        header.relay_us = now_us();
                        c.frames.fetch_add(1, Relaxed);
                        c.bytes.fetch_add(payload.len() as u64, Relaxed);
                        if header.index == 0 {
                            c.groups.fetch_add(1, Relaxed);
                        }
                        if header.flags & FLAG_CAPTURE_FROM_SEI != 0 {
                            c.capture_lat_sum_us
                                .fetch_add(header.publisher_us.saturating_sub(header.capture_us), Relaxed);
                            c.capture_lat_n.fetch_add(1, Relaxed);
                        }
                        let frame = MediaFrame {
                            broadcast: room_index,
                            track: h.track,
                            group: h.group,
                            codec: codec.clone(),
                            header,
                            payload,
                        };
                        if track.groups.push(frame) == tq_transport::track::Push::Dropped {
                            // Superseded by a newer group; stop reading this one.
                            reader.stop();
                            return;
                        }
                    }
                    Ok(None) => return,
                    // Reset by the sender (late or superseded group).
                    Err(_) => return,
                }
            }
        });
    }
}

/// The session's events about its own tracks.
async fn owner_events(mut rx: EventsRx, ctx: &Ctx) -> anyhow::Result<()> {
    let mut last = 0u64;
    while let Some(msg) = rx.recv().await? {
        let RoomMessage::Event(RoomEvent { seq, ts_us, event }) = msg else {
            continue;
        };
        if seq != last + 1 {
            // Harmless: the owner restates everything after a gap of its
            // own, and unchanged values are dropped.
            tracing::debug!(room = %ctx.room.id, expected = last + 1, seq, "gap in session events");
        }
        last = seq;
        if !event.from_owner() {
            tracing::warn!(room = %ctx.room.id, ?event, "session sent an event it does not own");
            continue;
        }
        ctx.room.owner_event(ctx.sid, event, ts_us);
    }
    Ok(())
}

/// Datagrams in both roles: channel messages and heartbeats this session
/// sends into broadcasts, acks and echoes it sends as an owner, and the
/// frames of its datagram tracks.
async fn datagrams(ctx: Ctx) -> anyhow::Result<()> {
    let mut dropped = 0u64;
    loop {
        let data = ctx.session.read_datagram().await?;
        match Datagram::peek_type(&data) {
            Some(tq_wire::DG_MESSAGE | tq_wire::DG_HEARTBEAT) => {
                match ctx.room.route_to_owner(ctx.sid, data.to_vec()) {
                    Ok((owner, bytes)) => {
                        let _ = owner.send_datagram(bytes);
                    }
                    Err(why) => {
                        dropped += 1;
                        if dropped.is_power_of_two() {
                            tracing::debug!(sid = ctx.sid, why, dropped, "channel datagram dropped");
                        }
                    }
                }
            }
            Some(tq_wire::DG_ACK | tq_wire::DG_ECHO) => {
                if let Some((target, bytes)) = ctx.room.route_to_sender(ctx.sid, data.to_vec()) {
                    let _ = target.send_datagram(bytes);
                }
            }
            Some(tq_wire::DG_TRACK_FRAME) if data.len() >= tq_wire::TRACK_FRAME_HEADER_LEN => {
                let owner_index = u16::from_be_bytes([data[1], data[2]]);
                let track_index = u16::from_be_bytes([data[3], data[4]]);
                let Some((room_index, info, track)) = ctx.room.owner_track(ctx.sid, owner_index, track_index) else {
                    continue;
                };
                if !info.is_datagram() {
                    continue;
                }
                let mut bytes = data.to_vec();
                tq_wire::set_track_frame_route(&mut bytes, room_index, now_us());
                let c = &track.counters;
                c.frames.fetch_add(1, Relaxed);
                c.bytes
                    .fetch_add((bytes.len() - tq_wire::TRACK_FRAME_HEADER_LEN) as u64, Relaxed);
                track.datagrams.push(Bytes::from(bytes));
            }
            _ => {}
        }
    }
}

// --- to the session: its subscriptions ------------------------------------

/// Sends one subscribed track for as long as the subscription lasts. Waits
/// while the track does not exist; follows it when its indices change;
/// starts over when it is replaced.
async fn subscription(ctx: Ctx, sub: Subscription) -> anyhow::Result<()> {
    let mut layout = ctx.room.layout.subscribe();
    loop {
        layout.borrow_and_update();
        let Some(r) = ctx.room.resolve(&sub.track) else {
            layout.changed().await?;
            continue;
        };
        if r.info.is_datagram() {
            send_datagrams(&ctx, &sub, r, &mut layout).await?;
        } else {
            send_groups(&ctx, &sub, r, &mut layout).await?;
        }
    }
}

/// The track's current indices if it is still the same relay track.
fn still_same(room: &Room, path: &Path, relay: &RelayTrack) -> Option<Resolved> {
    room.resolve(path)
        .filter(|r| Arc::ptr_eq(&r.relay.groups, &relay.groups))
}

/// Minimum spacing between messages for a `max_rate_hz` subscription.
fn rate_interval(sub: &Subscription, r: &Resolved) -> Option<Duration> {
    let thinnable = matches!(r.info.delivery(), Some(Delivery::Latest | Delivery::Datagram));
    sub.max_rate_hz
        .filter(|hz| thinnable && *hz > 0.0)
        .map(|hz| Duration::from_secs_f64(1.0 / hz))
}

async fn send_groups(
    ctx: &Ctx,
    sub: &Subscription,
    mut r: Resolved,
    layout: &mut tokio::sync::watch::Receiver<u64>,
) -> anyhow::Result<()> {
    let policy = match sub.policy {
        Policy::Realtime => ctx.relay.realtime_policy,
        Policy::Smooth => ctx.relay.smooth_policy,
    };
    let mut sender = GroupSender::new(ctx.session.clone(), policy, r.info.priority());
    let interval = rate_interval(sub, &r);
    let (cache, mut rx) = r.relay.groups.subscribe();
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut pending: Option<Arc<MediaFrame>> = None;

    let send = async |sender: &mut GroupSender, r: &Resolved, f: &MediaFrame| -> anyhow::Result<()> {
        // Frames carry the indices they arrived with; the advertisement
        // may have changed since. Trust the current ones.
        let mut f = f.clone();
        f.broadcast = r.broadcast;
        f.track = r.track;
        if sender.send(&f, f.header.relay_us).await? == Outcome::Late && r.info.kind == TrackKind::Video {
            ctx.room.request_keyframe(&sub.track, "relay -> subscriber late");
        }
        Ok(())
    };
    for f in &cache {
        send(&mut sender, &r, f).await?;
    }
    loop {
        let due = match (interval, last_sent, pending.is_some()) {
            (Some(i), Some(t), true) => Some(t + i),
            _ => None,
        };
        tokio::select! {
            changed = layout.changed() => {
                changed?;
                match still_same(&ctx.room, &sub.track, &r.relay) {
                    Some(now) => r = now,
                    None => return Ok(()),
                }
            }
            _ = sleep_until(due) => {
                if let Some(f) = pending.take() {
                    last_sent = Some(tokio::time::Instant::now());
                    send(&mut sender, &r, &f).await?;
                }
            }
            f = rx.recv() => match f {
                Ok(f) => {
                    if let Some(i) = interval {
                        let now = tokio::time::Instant::now();
                        if last_sent.is_some_and(|t| now < t + i) {
                            pending = Some(f);
                            continue;
                        }
                        last_sent = Some(now);
                        pending = None;
                    }
                    send(&mut sender, &r, &f).await?;
                }
                Err(RecvError::Lagged(_)) => {
                    sender.abandon();
                    if r.info.kind == TrackKind::Video {
                        ctx.room.request_keyframe(&sub.track, "subscriber lagged");
                    }
                }
                Err(RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn send_datagrams(
    ctx: &Ctx,
    sub: &Subscription,
    mut r: Resolved,
    layout: &mut tokio::sync::watch::Receiver<u64>,
) -> anyhow::Result<()> {
    let interval = rate_interval(sub, &r);
    let (latest, mut rx) = r.relay.datagrams.subscribe();
    if let Some(f) = latest {
        let _ = ctx.session.send_datagram(f);
    }
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut pending: Option<Bytes> = None;
    loop {
        let due = match (interval, last_sent, pending.is_some()) {
            (Some(i), Some(t), true) => Some(t + i),
            _ => None,
        };
        tokio::select! {
            changed = layout.changed() => {
                changed?;
                match still_same(&ctx.room, &sub.track, &r.relay) {
                    Some(now) => r = now,
                    None => return Ok(()),
                }
            }
            _ = sleep_until(due) => {
                if let Some(f) = pending.take() {
                    last_sent = Some(tokio::time::Instant::now());
                    let _ = ctx.session.send_datagram(f);
                }
            }
            f = rx.recv() => match f {
                Ok(f) => {
                    if let Some(i) = interval {
                        let now = tokio::time::Instant::now();
                        if last_sent.is_some_and(|t| now < t + i) {
                            pending = Some(f);
                            continue;
                        }
                        last_sent = Some(now);
                        pending = None;
                    }
                    let _ = ctx.session.send_datagram(f);
                }
                // Lost, as it would be on the network.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn sleep_until(due: Option<tokio::time::Instant>) {
    match due {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}
