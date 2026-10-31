//! A connection to a relay room.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering::Relaxed, Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use bytes::{Bytes, BytesMut};
use tokio::sync::{broadcast, broadcast::error::RecvError, mpsc, watch};
use tq_transport::{
    control::{ControlRx, ControlTx},
    events::{EventsRx, EventsTx, Incoming},
    group::{GroupReader, GroupSender, SendPolicy},
    quic::{self, QuicOptions, ServerTrust},
    wt,
};
use tq_wire::{
    clock_offset, now_us, AckStatus, BuildInfo, ClockFilter, Compat, Control, Datagram, FrameHeader, Offset,
    OwnerTrackStats, Path, Perms, RoomEvent, RoomMessage, RoomState, Subscription, ECHO_TRIPPED,
};

use crate::publication::{Holder, Link, LocalTrack, PublishOptions, Publication};

const PING_INTERVAL: Duration = Duration::from_millis(500);
const CLOCK_WINDOW: usize = 20;
/// How long a group stream for a track not yet in the room state waits for
/// the state that names it.
const STATE_WAIT: Duration = Duration::from_secs(2);
/// Frames queued per subscription before new ones are dropped.
const SUBSCRIPTION_QUEUE: usize = 512;
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct Options {
    /// `https://host:port` of the relay's WebTransport endpoint.
    pub relay_url: url::Url,
    pub room: String,
    pub token: String,
    pub trust: ServerTrust,
    pub quic: QuicOptions,
    /// This program, as the room will show it.
    pub build: BuildInfo,
}

/// One frame of a subscribed track.
#[derive(Debug, Clone)]
pub struct Frame {
    pub track: Path,
    /// 0 for datagram tracks.
    pub group: u64,
    /// The WebCodecs string of a video group, or a data track's encoding.
    pub codec: Arc<str>,
    /// For datagram tracks, `index` is 0 and `frame_id` is the sequence
    /// number.
    pub header: FrameHeader,
    pub payload: Bytes,
    /// When it arrived, this machine's clock.
    pub recv_us: u64,
    /// The clock offsets known when it arrived.
    pub clocks: Clocks,
}

impl Frame {
    pub fn is_key(&self) -> bool {
        self.header.is_key()
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    /// When it was captured, on this machine's clock (Unix µs). `None`
    /// until the clock offsets are measured, just after connecting; see
    /// [`Session::wait_owner_offset`]. A video frame's capture time is
    /// the owner's receive time unless `FLAG_CAPTURE_FROM_SEI` is set.
    pub fn capture_local_us(&self) -> Option<u64> {
        Some(self.clocks.owner?.to_local(self.header.capture_us))
    }

    /// Its time on each hop, every number between two clocks converted.
    pub fn hops(&self) -> Hops {
        let h = &self.header;
        let ms = |a: u64, b: u64| (a as f64 - b as f64) / 1000.0;
        let relay_here = self.clocks.relay.map(|o| o.to_local(h.relay_us));
        let owner = self.clocks.owner;
        Hops {
            capture_to_publisher_ms: ms(h.publisher_us, h.capture_us),
            publisher_to_relay_ms: relay_here
                .zip(owner)
                .map(|(r, o)| ms(r, o.to_local(h.publisher_us))),
            relay_to_here_ms: relay_here.map(|r| ms(self.recv_us, r)),
            capture_to_here_ms: owner.map(|o| ms(self.recv_us, o.to_local(h.capture_us))),
        }
    }
}

/// Clock offsets: relay − this machine, and the frame's owner − this
/// machine (both hops composed).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Clocks {
    pub relay: Option<Offset>,
    pub owner: Option<Offset>,
}

impl Clocks {
    /// The error bound of anything compared across owner and here, µs.
    pub fn error_us(&self) -> Option<i64> {
        self.owner.map(|o| o.error_us)
    }
}

/// A frame's latency per hop, ms. The owner hop is on one clock; the
/// rest are `None` until the clock offsets are known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hops {
    pub capture_to_publisher_ms: f64,
    pub publisher_to_relay_ms: Option<f64>,
    pub relay_to_here_ms: Option<f64>,
    pub capture_to_here_ms: Option<f64>,
}

/// A lock claim the relay refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    /// Who holds it, if anyone.
    pub holder: Option<String>,
    pub reason: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.holder {
            Some(h) => write!(f, "{} (held by {h})", self.reason),
            None => f.write_str(&self.reason),
        }
    }
}

impl std::error::Error for Refused {}

/// An ack or echo for something this session sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ack {
        channel: Path,
        seq: u32,
        rtt_us: u64,
        status: AckStatus,
        detail: String,
    },
    Echo {
        lock: Path,
        seq: u32,
        rtt_us: u64,
        /// The lock's watchdog is tripped.
        tripped: bool,
    },
}

struct SubEntry {
    tx: mpsc::Sender<Frame>,
    newest_group: u64,
}

struct Inner {
    wt: wt::Session,
    out: mpsc::UnboundedSender<Control>,
    id: String,
    sid: u32,
    perms: Perms,
    relay: BuildInfo,
    room: watch::Sender<Arc<RoomState>>,
    events: broadcast::Sender<RoomEvent>,
    control: broadcast::Sender<Control>,
    replies: broadcast::Sender<Reply>,
    clock: Mutex<ClockFilter>,
    subs: Mutex<HashMap<Path, SubEntry>>,
    /// Published here, by owner hop index, and by name.
    pubs: Mutex<Vec<Arc<Publication>>>,
    seqs: Mutex<HashMap<Path, u32>>,
    closed: watch::Sender<Option<String>>,
}

impl Inner {
    fn send(&self, msg: Control) {
        let _ = self.out.send(msg);
    }

    /// The path of a track by its room indices, from the latest state.
    fn path_of(&self, broadcast: u16, track: u16) -> Option<(Path, Arc<str>)> {
        let room = self.room.borrow();
        let b = room.broadcast_at(broadcast)?;
        let t = b.tracks.iter().find(|t| t.index == track)?;
        let codec = t.info.data.as_ref().map_or("", |d| d.encoding.as_str());
        Some((Path::new(&b.name, &t.info.id), codec.into()))
    }

    fn publication(&self, name: &str) -> Option<Arc<Publication>> {
        self.pubs.lock().unwrap().iter().find(|p| p.name() == name).cloned()
    }

    fn relay_offset(&self) -> Option<Offset> {
        self.clock.lock().unwrap().estimate()
    }

    fn owner_offset(&self, broadcast: &str) -> Option<Offset> {
        let owner = {
            let room = self.room.borrow();
            let b = room.broadcast(broadcast)?;
            Offset {
                offset_us: b.clock_offset_us?,
                error_us: b.clock_error_us.unwrap_or(0),
            }
        };
        Some(self.relay_offset()?.then(owner))
    }

    fn clocks(&self, broadcast: &str) -> Clocks {
        Clocks {
            relay: self.relay_offset(),
            owner: self.owner_offset(broadcast),
        }
    }

    fn publication_at(&self, owner_index: u16) -> Option<Arc<Publication>> {
        self.pubs.lock().unwrap().get(owner_index as usize).cloned()
    }

    fn close(&self, reason: String) {
        self.closed.send_if_modified(|c| {
            if c.is_none() {
                *c = Some(reason);
                true
            } else {
                false
            }
        });
    }
}

/// A connection to a relay room. Cheap to clone; every clone is the same
/// session. It ends when the relay or the network ends it, or on
/// [`Session::close`]; see [`Session::closed`].
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Connects, says hello and waits for the relay's welcome.
    pub async fn connect(opts: Options) -> anyhow::Result<Self> {
        let client = quic::client(opts.trust.clone(), &opts.quic)?;
        let mut url = opts.relay_url.clone();
        url.set_path(&format!("/v1/room/{}", opts.room));
        url.query_pairs_mut().append_pair("token", &opts.token);
        let wt = tokio::time::timeout(Duration::from_secs(5), client.connect(url))
            .await
            .context("connect timed out")?
            .context("WebTransport connect")?;
        let (send, recv) = wt.open_bi().await?;
        let ctl = ControlTx::new(send);
        let mut rx = ControlRx::new(recv);
        ctl.send(&Control::Hello {
            build: opts.build.clone(),
        })
        .await?;
        let welcome = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        let (id, sid, perms, relay) = match welcome {
            Ok(Ok(Some(Control::Welcome {
                session,
                sid,
                perms,
                relay,
            }))) => (session, sid, perms, relay),
            Ok(Ok(Some(Control::Error { message }))) => anyhow::bail!("relay refused: {message}"),
            Ok(Ok(Some(other))) => anyhow::bail!("expected welcome, got {other:?}"),
            // The relay closes the session to refuse it, with the reason.
            Ok(Ok(None)) | Ok(Err(_)) => {
                let why = tokio::time::timeout(Duration::from_secs(1), wt.closed()).await;
                match why {
                    Ok(e) => anyhow::bail!("relay closed the session: {e}"),
                    Err(_) => anyhow::bail!("relay closed the control stream"),
                }
            }
            Err(_) => anyhow::bail!("no welcome from the relay"),
        };
        match Compat::of_build(Some(&relay)) {
            Compat::Same => {}
            Compat::MinorDiffers => {
                tracing::warn!("{}", tq_wire::mismatch("relay", Some(&relay), &opts.build.component))
            }
            Compat::Incompatible => {
                let reason = tq_wire::mismatch("relay", Some(&relay), &opts.build.component);
                wt.close(tq_wire::code::UNSUPPORTED_VERSION, reason.as_bytes());
                return Err(crate::Incompatible(reason).into());
            }
        }

        let (out, out_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            wt: wt.clone(),
            out,
            id,
            sid,
            perms,
            relay,
            room: watch::channel(Arc::new(RoomState::default())).0,
            events: broadcast::channel(256).0,
            control: broadcast::channel(256).0,
            replies: broadcast::channel(1024).0,
            clock: Mutex::new(ClockFilter::new(CLOCK_WINDOW)),
            subs: Default::default(),
            pubs: Default::default(),
            seqs: Default::default(),
            closed: watch::channel(None).0,
        });
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(outbox(ctl, out_rx));
        tasks.spawn(read_control(inner.clone(), rx));
        tasks.spawn(read_streams(inner.clone()));
        tasks.spawn(read_datagrams(inner.clone()));
        tasks.spawn(pinger(inner.clone()));
        let supervisor = inner.clone();
        tokio::spawn(async move {
            let mut app_closed = supervisor.closed.subscribe();
            let reason = tokio::select! {
                e = supervisor.wt.closed() => format!("closed: {e}"),
                Some(r) = tasks.join_next() => match r {
                    Ok(Ok(())) => "ended".to_string(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("{e}"),
                },
                _ = app_closed.wait_for(Option::is_some) => "closed by the app".to_string(),
            };
            tasks.abort_all();
            supervisor.wt.close(tq_wire::code::SHUTDOWN, b"bye");
            supervisor.subs.lock().unwrap().clear();
            supervisor.close(reason);
        });
        Ok(Self { inner })
    }

    /// This session's id, as the room shows it.
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn sid(&self) -> u32 {
        self.inner.sid
    }

    pub fn perms(&self) -> &Perms {
        &self.inner.perms
    }

    /// The relay's build.
    pub fn relay(&self) -> &BuildInfo {
        &self.inner.relay
    }

    pub fn transport(&self) -> &wt::Session {
        &self.inner.wt
    }

    /// The room's state, updated as the relay sends it.
    pub fn room(&self) -> watch::Receiver<Arc<RoomState>> {
        self.inner.room.subscribe()
    }

    /// The room's events from now on.
    pub fn events(&self) -> broadcast::Receiver<RoomEvent> {
        self.inner.events.subscribe()
    }

    /// Every control message from the relay from now on, except pings.
    pub fn control(&self) -> broadcast::Receiver<Control> {
        self.inner.control.subscribe()
    }

    /// Acks and echoes for what this session sends.
    pub fn replies(&self) -> broadcast::Receiver<Reply> {
        self.inner.replies.subscribe()
    }

    /// Relay clock minus this machine's clock.
    pub fn relay_offset(&self) -> Option<Offset> {
        self.inner.relay_offset()
    }

    /// Round trip of the latest ping to the relay, ms.
    pub fn relay_rtt_ms(&self) -> Option<f64> {
        self.inner.clock.lock().unwrap().last_rtt_us().map(|us| us as f64 / 1000.0)
    }

    /// A broadcast owner's clock minus this machine's clock, from both
    /// hops' offsets; its error is the sum of both bounds.
    pub fn owner_offset(&self, broadcast: &str) -> Option<Offset> {
        self.inner.owner_offset(broadcast)
    }

    /// [`Session::owner_offset`], waiting up to `timeout` for it: just
    /// after connecting, frames (such as the newest group the relay
    /// replays) can arrive before the first ping has come back.
    pub async fn wait_owner_offset(&self, broadcast: &str, timeout: Duration) -> Option<Offset> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(o) = self.owner_offset(broadcast) {
                return Some(o);
            }
            if tokio::time::Instant::now() >= deadline || self.is_closed() {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Resolves when the session has ended, with why.
    pub async fn closed(&self) -> String {
        let mut rx = self.inner.closed.subscribe();
        let r = rx.wait_for(Option::is_some).await;
        r.ok().and_then(|c| c.clone()).unwrap_or_default()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.borrow().is_some()
    }

    pub fn close(&self) {
        self.inner.close("closed by the app".into());
    }

    /// Sends a raw control message.
    pub fn send_control(&self, msg: Control) {
        self.inner.send(msg);
    }

    /// Waits for the first control message `f` picks, sending `msg` first.
    async fn request<T>(&self, msg: Control, mut f: impl FnMut(&Control) -> Option<T>) -> anyhow::Result<T> {
        let mut rx = self.inner.control.subscribe();
        self.inner.send(msg);
        tokio::time::timeout(REPLY_TIMEOUT, async {
            loop {
                match rx.recv().await {
                    Ok(m) => {
                        if let Some(t) = f(&m) {
                            return Ok(t);
                        }
                    }
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => anyhow::bail!("session closed"),
                }
            }
        })
        .await
        .context("no reply from the relay")?
    }

    // --- subscribing ----------------------------------------------------

    /// Subscribes to a track. Frames arrive on the returned reader;
    /// dropping it unsubscribes. A track that does not exist yet is waited
    /// for.
    pub async fn subscribe(&self, sub: impl Into<Subscription>) -> anyhow::Result<TrackReader> {
        let sub = sub.into();
        let path = sub.track.clone();
        let (tx, rx) = mpsc::channel(SUBSCRIPTION_QUEUE);
        self.inner
            .subs
            .lock()
            .unwrap()
            .insert(path.clone(), SubEntry { tx, newest_group: 0 });
        let want = path.clone();
        let result = self
            .request(Control::Subscribe { tracks: vec![sub] }, |m| match m {
                Control::SubscribeResult { track, ok, reason } if *track == want => Some((*ok, reason.clone())),
                _ => None,
            })
            .await;
        match result {
            Ok((true, _)) => Ok(TrackReader {
                path,
                rx,
                inner: self.inner.clone(),
            }),
            Ok((false, reason)) => {
                self.inner.subs.lock().unwrap().remove(&path);
                anyhow::bail!("subscribe {path}: {}", reason.unwrap_or_default())
            }
            Err(e) => {
                self.inner.subs.lock().unwrap().remove(&path);
                Err(e)
            }
        }
    }

    pub fn request_keyframe(&self, track: &Path, reason: &str) {
        self.inner.send(Control::KeyframeRequest {
            track: track.clone(),
            reason: reason.to_string(),
        });
    }

    // --- channels and locks ---------------------------------------------

    /// A sender for a channel of some broadcast.
    pub fn channel(&self, path: Path) -> Channel {
        Channel {
            path,
            inner: self.inner.clone(),
        }
    }

    /// Sends a heartbeat for a lock this session holds. The echo comes back
    /// as [`Reply::Echo`].
    pub fn heartbeat(&self, lock: &Path) -> anyhow::Result<u32> {
        let (broadcast, index) = {
            let room = self.inner.room.borrow();
            let b = room
                .broadcast(&lock.broadcast)
                .with_context(|| format!("no broadcast '{}'", lock.broadcast))?;
            (b.index, b.lock(&lock.name).with_context(|| format!("no lock '{lock}'"))?.index)
        };
        let seq = next_seq(&self.inner, lock);
        let d = Datagram::Heartbeat {
            sid: 0,
            broadcast,
            lock: index,
            seq,
            sent_us: now_us(),
        };
        self.inner.wt.send_datagram(d.encode())?;
        Ok(seq)
    }

    /// Claims a lock. `Ok(Err(_))` when someone else holds it (or the
    /// relay refused). Holding it means heartbeating it; [`Session::hold`]
    /// does both.
    pub async fn claim(&self, lock: Path, force: bool) -> anyhow::Result<Result<(), Refused>> {
        let want = lock.clone();
        self.request(Control::Claim { lock, force }, |m| match m {
            Control::ClaimResult {
                lock,
                ok,
                holder,
                reason,
            } if *lock == want => Some(match ok {
                true => Ok(()),
                false => Err(Refused {
                    holder: holder.clone(),
                    reason: reason.clone().unwrap_or_default(),
                }),
            }),
            _ => None,
        })
        .await
    }

    /// Releases a lock; false if this session did not hold it.
    pub async fn release(&self, lock: Path) -> anyhow::Result<bool> {
        let want = lock.clone();
        self.request(Control::Release { lock }, |m| match m {
            Control::ClaimResult { lock, ok, .. } if *lock == want => Some(*ok),
            _ => None,
        })
        .await
    }

    /// Claims a lock and heartbeats it every `interval` until the returned
    /// [`Hold`] is dropped (which releases it) or the lock is lost: taken by
    /// force, or this session ended.
    pub async fn hold(&self, lock: Path, force: bool, interval: Duration) -> anyhow::Result<Result<Hold, Refused>> {
        // Subscribed first, so a forced take right after the claim is seen.
        let events = self.events();
        if let Err(refused) = self.claim(lock.clone(), force).await? {
            return Ok(Err(refused));
        }
        let (held_tx, held) = watch::channel(true);
        let task = tokio::spawn(heartbeat(self.clone(), lock.clone(), interval, events, held_tx));
        Ok(Ok(Hold {
            lock,
            session: self.clone(),
            held,
            task,
        }))
    }

    // --- publishing -----------------------------------------------------

    /// Publishes a broadcast on this session, with default options.
    pub async fn publish(&self, p: &Arc<Publication>) -> anyhow::Result<()> {
        self.publish_with(p, PublishOptions::default()).await
    }

    /// Advertises `p` and starts sending its tracks as they are subscribed
    /// to. Its channel messages arrive in its inbox until this session
    /// ends.
    pub async fn publish_with(&self, p: &Arc<Publication>, opts: PublishOptions) -> anyhow::Result<()> {
        let name = p.name().to_string();
        {
            let mut pubs = self.inner.pubs.lock().unwrap();
            anyhow::ensure!(
                !pubs.iter().any(|x| x.name() == name),
                "'{name}' is already published on this session"
            );
            pubs.push(p.clone());
        }
        let want = name.clone();
        let reply = self
            .request(Control::Advertise(p.advertisement().clone()), |m| match m {
                Control::AdvertiseOk { broadcast, index, .. } if *broadcast == want => Some(Ok(*index)),
                Control::AdvertiseError { broadcast, reason } if *broadcast == want => Some(Err(reason.clone())),
                _ => None,
            })
            .await;
        let owner_index = match reply {
            Ok(Ok(i)) => i,
            Ok(Err(reason)) => {
                self.inner.pubs.lock().unwrap().retain(|x| x.name() != name);
                anyhow::bail!("relay refused to publish '{name}': {reason}");
            }
            Err(e) => {
                self.inner.pubs.lock().unwrap().retain(|x| x.name() != name);
                return Err(e);
            }
        };
        // The relay numbers a session's broadcasts in advertise order, as
        // `pubs` is.
        let position = self.inner.pubs.lock().unwrap().iter().position(|x| x.name() == name);
        anyhow::ensure!(
            position == Some(owner_index as usize),
            "relay gave '{name}' index {owner_index}, expected {position:?}"
        );
        p.attach(Link {
            session: self.inner.wt.clone(),
            owner_index,
        });

        let policy = SendPolicy {
            keep_groups: 0,
            max_lag: opts.max_lag,
        };
        let mut tasks = tokio::task::JoinSet::new();
        for (i, t) in p.tracks.iter().enumerate() {
            let info = p.advertisement().tracks[i].clone();
            match t {
                LocalTrack::Groups(g) => {
                    tasks.spawn(send_group_track(
                        self.inner.wt.clone(),
                        p.clone(),
                        g.clone(),
                        info,
                        i as u16,
                        owner_index,
                        policy,
                    ));
                }
                LocalTrack::Datagrams(d) => {
                    tasks.spawn(send_datagram_track(
                        self.inner.wt.clone(),
                        p.clone(),
                        d.clone(),
                        info.id,
                        owner_index,
                    ));
                }
            }
        }
        tasks.spawn(send_edges(self.inner.wt.clone(), p.clone()));
        tasks.spawn(send_stats(self.inner.clone(), p.clone()));
        let inner = self.inner.clone();
        let p = p.clone();
        tokio::spawn(async move {
            let mut closed = inner.closed.subscribe();
            tokio::select! {
                _ = closed.wait_for(Option::is_some) => {}
                Some(Ok(Err(e))) = tasks.join_next() => {
                    tracing::warn!(broadcast = %p.name(), error = format!("{e:#}"), "publishing failed");
                }
            }
            tasks.abort_all();
            p.detach(&inner.wt);
        });
        tracing::info!(broadcast = %name, index = owner_index, "published");
        Ok(())
    }
}

fn next_seq(inner: &Inner, path: &Path) -> u32 {
    let mut seqs = inner.seqs.lock().unwrap();
    let s = seqs.entry(path.clone()).or_insert(0);
    *s = s.wrapping_add(1);
    *s
}

/// A lock this session holds, heartbeated in the background. Dropping it
/// releases the lock (without waiting for the relay).
pub struct Hold {
    lock: Path,
    session: Session,
    held: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Hold {
    pub fn lock(&self) -> &Path {
        &self.lock
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Whether the lock is still this session's.
    pub fn is_held(&self) -> bool {
        *self.held.borrow()
    }

    /// Resolves once the lock is lost: taken by force, or the session
    /// ended. Heartbeats have stopped by then.
    pub async fn lost(&self) {
        let mut held = self.held.clone();
        let _ = held.wait_for(|h| !h).await;
    }

    /// Releases the lock and waits for the relay to confirm.
    pub async fn release(self) -> anyhow::Result<bool> {
        self.task.abort();
        if !self.is_held() {
            return Ok(false);
        }
        let (session, lock) = (self.session.clone(), self.lock.clone());
        // Released here; `Drop` must not send it again.
        std::mem::forget(self);
        session.release(lock).await
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.task.abort();
        if self.is_held() {
            self.session.inner.send(Control::Release {
                lock: self.lock.clone(),
            });
        }
    }
}

/// Heartbeats `lock` until it is no longer this session's.
async fn heartbeat(
    session: Session,
    lock: Path,
    interval: Duration,
    mut events: broadcast::Receiver<RoomEvent>,
    held: watch::Sender<bool>,
) {
    let me = session.id().to_string();
    let mut room = session.room();
    let mut tick = tokio::time::interval(interval);
    // The claim's result and the state that shows it travel on different
    // streams: the state counts only once it has shown this session.
    let mut seen = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if session.heartbeat(&lock).is_err() && session.is_closed() {
                    break;
                }
            }
            r = room.changed() => {
                if r.is_err() {
                    break;
                }
                let holder = room.borrow_and_update().lock(&lock).map(|l| l.holder.clone());
                match holder {
                    Some(Some(h)) if h == me => seen = true,
                    // Gone from the room, or someone else's (or free) after
                    // having been ours.
                    None => break,
                    Some(_) if seen => break,
                    Some(_) => {}
                }
            }
            e = events.recv() => match e {
                Ok(e) => {
                    if let tq_wire::Event::LockChanged { lock: l, from: Some(from), .. } = &e.event {
                        if *l == lock && *from == me {
                            break;
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            _ = session.closed() => break,
        }
    }
    held.send_replace(false);
}

/// Frames of one subscribed track. Dropping it unsubscribes.
pub struct TrackReader {
    path: Path,
    rx: mpsc::Receiver<Frame>,
    inner: Arc<Inner>,
}

impl TrackReader {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next frame; `None` once the session has ended.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Frame> {
        self.rx.try_recv().ok()
    }
}

impl Drop for TrackReader {
    fn drop(&mut self) {
        self.inner.subs.lock().unwrap().remove(&self.path);
        self.inner.send(Control::Unsubscribe {
            tracks: vec![self.path.clone()],
        });
    }
}

/// Sends messages on one channel of some broadcast.
#[derive(Clone)]
pub struct Channel {
    path: Path,
    inner: Arc<Inner>,
}

impl Channel {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sends one message; returns its sequence number. Its ack comes back
    /// as [`Reply::Ack`]. Fails if the channel is not in the room, or the
    /// message does not fit in a datagram.
    pub fn send(&self, payload: &[u8]) -> anyhow::Result<u32> {
        let (broadcast, channel) = {
            let room = self.inner.room.borrow();
            let b = room
                .broadcast(&self.path.broadcast)
                .with_context(|| format!("no broadcast '{}'", self.path.broadcast))?;
            let c = b
                .channel(&self.path.name)
                .with_context(|| format!("no channel '{}'", self.path))?;
            (b.index, c.index)
        };
        anyhow::ensure!(
            tq_wire::MESSAGE_HEADER_LEN + payload.len() <= tq_wire::MAX_DATAGRAM,
            "message of {} bytes does not fit in a datagram",
            payload.len()
        );
        let seq = next_seq(&self.inner, &self.path);
        let d = Datagram::Message {
            sid: 0,
            broadcast,
            channel,
            seq,
            sent_us: now_us(),
            payload: Bytes::copy_from_slice(payload),
        };
        self.inner.wt.send_datagram(d.encode())?;
        Ok(seq)
    }

    pub fn send_json<T: serde::Serialize + ?Sized>(&self, value: &T) -> anyhow::Result<u32> {
        self.send(&serde_json::to_vec(value)?)
    }
}

// --- connection tasks -----------------------------------------------------

async fn outbox(ctl: ControlTx, mut rx: mpsc::UnboundedReceiver<Control>) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await {
        ctl.send(&msg).await?;
    }
    Ok(())
}

async fn pinger(inner: Arc<Inner>) -> anyhow::Result<()> {
    let mut t = tokio::time::interval(PING_INTERVAL);
    let mut id = 0u64;
    loop {
        t.tick().await;
        id += 1;
        inner.send(Control::Ping { id, t0_us: now_us() });
    }
}

async fn read_control(inner: Arc<Inner>, mut rx: ControlRx) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match &msg {
            Control::Ping { id, t0_us } => {
                inner.send(Control::Pong {
                    id: *id,
                    t0_us: *t0_us,
                    t1_us: now_us(),
                });
                continue;
            }
            Control::Pong { t0_us, t1_us, .. } => {
                let (relay_minus_me, rtt) = clock_offset(*t0_us, *t1_us, now_us());
                inner.clock.lock().unwrap().add(relay_minus_me, rtt);
                continue;
            }
            Control::LockHolder { lock, sid, session } => {
                if let Some(p) = inner.publication(&lock.broadcast) {
                    let holder = sid.map(|sid| Holder {
                        sid,
                        session: session.clone().unwrap_or_default(),
                    });
                    p.set_holder(&lock.name, holder);
                }
            }
            Control::TrackDemand { track, subscribers } => {
                if let Some(p) = inner.publication(&track.broadcast) {
                    p.set_demand(&track.name, *subscribers);
                }
            }
            Control::KeyframeRequest { track, .. } => {
                if let Some(p) = inner.publication(&track.broadcast) {
                    p.request_keyframe(&track.name);
                }
            }
            Control::Error { message } => tracing::warn!(%message, "relay error"),
            _ => {}
        }
        let _ = inner.control.send(msg);
    }
    anyhow::bail!("relay closed the control stream")
}

async fn read_streams(inner: Arc<Inner>) -> anyhow::Result<()> {
    loop {
        let stream = inner.wt.accept_uni().await?;
        let inner = inner.clone();
        tokio::spawn(async move {
            match Incoming::open(stream).await {
                Ok(Incoming::Group(reader)) => {
                    if let Err(e) = read_group(inner, reader).await {
                        tracing::trace!(error = format!("{e:#}"), "group ended");
                    }
                }
                Ok(Incoming::Events(rx)) => {
                    if let Err(e) = read_events(inner, rx).await {
                        tracing::debug!(error = format!("{e:#}"), "events stream ended");
                    }
                }
                Err(e) => tracing::debug!(error = format!("{e:#}"), "bad stream"),
            }
        });
    }
}

async fn read_events(inner: Arc<Inner>, mut rx: EventsRx) -> anyhow::Result<()> {
    while let Some(msg) = rx.recv().await? {
        match msg {
            RoomMessage::State { state, .. } => {
                inner.room.send_replace(Arc::new(state));
            }
            RoomMessage::Event(e) => {
                let _ = inner.events.send(e);
            }
            RoomMessage::Unknown => {}
        }
    }
    Ok(())
}

async fn read_group(inner: Arc<Inner>, mut reader: GroupReader) -> anyhow::Result<()> {
    let h = reader.header.clone();
    let mut found = inner.path_of(h.broadcast, h.track);
    if found.is_none() {
        // A new track can arrive before the state that names it.
        let mut room = inner.room.subscribe();
        let _ = tokio::time::timeout(STATE_WAIT, async {
            while room.changed().await.is_ok() {
                if inner.path_of(h.broadcast, h.track).is_some() {
                    break;
                }
            }
        })
        .await;
        found = inner.path_of(h.broadcast, h.track);
    }
    let Some((path, _)) = found else {
        reader.stop();
        return Ok(());
    };
    // A newer group supersedes older ones; nothing in them is needed.
    let tx = {
        let mut subs = inner.subs.lock().unwrap();
        let Some(e) = subs.get_mut(&path) else {
            reader.stop();
            return Ok(());
        };
        if e.newest_group > h.group {
            reader.stop();
            return Ok(());
        }
        e.newest_group = h.group;
        e.tx.clone()
    };
    let codec: Arc<str> = h.codec.as_str().into();
    while let Some((header, payload)) = reader.next().await? {
        let current = inner
            .subs
            .lock()
            .unwrap()
            .get(&path)
            .is_some_and(|e| e.newest_group == h.group);
        if !current {
            reader.stop();
            return Ok(());
        }
        let frame = Frame {
            recv_us: now_us(),
            clocks: inner.clocks(&path.broadcast),
            track: path.clone(),
            group: h.group,
            codec: codec.clone(),
            header,
            payload,
        };
        match tx.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(track = %path, "subscriber not keeping up, frame dropped");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                reader.stop();
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn read_datagrams(inner: Arc<Inner>) -> anyhow::Result<()> {
    loop {
        let data = inner.wt.read_datagram().await?;
        let recv_us = now_us();
        let d = match Datagram::decode(data) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "bad datagram");
                continue;
            }
        };
        match d {
            Datagram::TrackFrame {
                broadcast,
                track,
                seq,
                flags,
                capture_us,
                publisher_us,
                relay_us,
                payload,
            } => {
                let Some((path, codec)) = inner.path_of(broadcast, track) else {
                    continue;
                };
                let tx = inner.subs.lock().unwrap().get(&path).map(|e| e.tx.clone());
                if let Some(tx) = tx {
                    let _ = tx.try_send(Frame {
                        clocks: inner.clocks(&path.broadcast),
                        track: path,
                        group: 0,
                        codec,
                        header: FrameHeader {
                            index: 0,
                            flags,
                            capture_us,
                            publisher_us,
                            relay_us,
                            frame_id: seq,
                        },
                        payload,
                        recv_us,
                    });
                }
            }
            Datagram::Ack {
                broadcast,
                channel,
                seq,
                echo_us,
                status,
                detail,
                ..
            } => {
                let path = {
                    let room = inner.room.borrow();
                    room.broadcast_at(broadcast).and_then(|b| {
                        let c = b.channels.iter().find(|c| c.index == channel)?;
                        Some(Path::new(&b.name, &c.info.id))
                    })
                };
                if let Some(channel) = path {
                    let _ = inner.replies.send(Reply::Ack {
                        channel,
                        seq,
                        rtt_us: recv_us.saturating_sub(echo_us),
                        status,
                        detail,
                    });
                }
            }
            Datagram::Echo {
                broadcast,
                lock,
                seq,
                echo_us,
                flags,
                ..
            } => {
                let path = {
                    let room = inner.room.borrow();
                    room.broadcast_at(broadcast).and_then(|b| {
                        let l = b.locks.iter().find(|l| l.index == lock)?;
                        Some(Path::new(&b.name, &l.info.id))
                    })
                };
                if let Some(lock) = path {
                    let _ = inner.replies.send(Reply::Echo {
                        lock,
                        seq,
                        rtt_us: recv_us.saturating_sub(echo_us),
                        tripped: flags & ECHO_TRIPPED != 0,
                    });
                }
            }
            // Into one of our broadcasts, on the owner hop.
            Datagram::Message {
                sid,
                broadcast,
                channel,
                seq,
                sent_us,
                payload,
            } => {
                if let Some(p) = inner.publication_at(broadcast) {
                    p.message(channel, sid, seq, sent_us, payload);
                }
            }
            Datagram::Heartbeat {
                sid,
                broadcast,
                lock,
                seq,
                sent_us,
            } => {
                if let Some(echo) = inner.publication_at(broadcast).and_then(|p| p.heartbeat(lock, sid, seq, sent_us)) {
                    let _ = inner.wt.send_datagram(echo.encode());
                }
            }
        }
    }
}

// --- publishing tasks -----------------------------------------------------

/// Sends one group track to the relay while anyone subscribes to it.
async fn send_group_track(
    wt: wt::Session,
    p: Arc<Publication>,
    track: Arc<tq_transport::track::Track>,
    info: tq_wire::TrackInfo,
    index: u16,
    owner_index: u16,
    policy: SendPolicy,
) -> anyhow::Result<()> {
    let mut demand = p.demand();
    let wanted = |d: &HashMap<String, u32>| d.get(&info.id).copied().unwrap_or(0) > 0;
    loop {
        demand.wait_for(wanted).await?;
        let mut sender = GroupSender::new(wt.clone(), policy, info.priority());
        let (cache, mut rx) = track.subscribe();
        let stamp = |f: &tq_wire::MediaFrame| {
            let mut f = f.clone();
            f.broadcast = owner_index;
            f.track = index;
            f
        };
        for f in &cache {
            sender.send(&stamp(f), f.header.publisher_us).await?;
        }
        loop {
            tokio::select! {
                r = demand.changed() => {
                    r?;
                    if !wanted(&demand.borrow_and_update()) {
                        sender.abandon();
                        break;
                    }
                }
                f = rx.recv() => match f {
                    Ok(f) => {
                        let outcome = sender.send(&stamp(&f), f.header.publisher_us).await?;
                        if outcome != tq_transport::group::Outcome::Sent {
                            p.add_dropped(&info.id, 1);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        p.add_dropped(&info.id, n);
                        sender.abandon();
                    }
                    Err(RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

/// Sends one datagram track's frames while anyone subscribes to it.
async fn send_datagram_track(
    wt: wt::Session,
    p: Arc<Publication>,
    track: Arc<tq_transport::dgtrack::DatagramTrack>,
    id: String,
    owner_index: u16,
) -> anyhow::Result<()> {
    let demand = p.demand();
    let (_, mut rx) = track.subscribe();
    loop {
        match rx.recv().await {
            Ok(f) => {
                if demand.borrow().get(&id).copied().unwrap_or(0) == 0 {
                    continue;
                }
                let mut b = BytesMut::from(&f[..]);
                tq_wire::set_track_frame_route(&mut b, owner_index, 0);
                let _ = wt.send_datagram(b.freeze());
            }
            Err(RecvError::Lagged(n)) => p.add_dropped(&id, n),
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

/// The publication's track edges to the relay: first the current value of
/// each (the relay drops those that change nothing), then every edge as it
/// happens, and everything again after falling behind.
async fn send_edges(wt: wt::Session, p: Arc<Publication>) -> anyhow::Result<()> {
    // Subscribed before reading the current values, so nothing falls in
    // between.
    let mut rx = p.subscribe_edges();
    let mut tx = EventsTx::open(&wt).await?;
    let mut seq = 0u64;
    let mut send = async |tx: &mut EventsTx, event, ts_us| {
        seq += 1;
        tx.send(&RoomMessage::Event(RoomEvent { seq, ts_us, event })).await
    };
    for e in p.current_edges() {
        send(&mut tx, e, now_us()).await?;
    }
    loop {
        match rx.recv().await {
            Ok((e, ts_us)) => send(&mut tx, e, ts_us).await?,
            Err(RecvError::Lagged(_)) => {
                for e in p.current_edges() {
                    send(&mut tx, e, now_us()).await?;
                }
            }
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

async fn send_stats(inner: Arc<Inner>, p: Arc<Publication>) -> anyhow::Result<()> {
    let mut t = tokio::time::interval(STATS_INTERVAL);
    loop {
        t.tick().await;
        let tracks = p
            .advertisement()
            .tracks
            .iter()
            .zip(&p.dropped)
            .map(|(t, d)| OwnerTrackStats {
                id: t.id.clone(),
                dropped: d.load(Relaxed),
            })
            .collect();
        inner.send(Control::TrackStats {
            broadcast: p.name().to_string(),
            tracks,
        });
    }
}
