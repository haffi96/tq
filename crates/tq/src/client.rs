//! A connection to a relay room that comes back by itself.
//!
//! A [`Client`] connects, and after every drop connects again with
//! backoff. What a program sets up on it outlives each session:
//! subscriptions ([`Client::subscribe`]) resubscribe, publications
//! ([`Client::publish`]) are published again, and the room's state, events
//! and replies come through one set of receivers. Locks do not carry over:
//! the relay frees a session's locks when it ends, and a program claims
//! them again on purpose (on [`Conn::Connected`]).

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::{broadcast, broadcast::error::RecvError, mpsc, watch};
use tq_wire::{Control, Path, RoomEvent, RoomState, Subscription};

use crate::{Frame, Incompatible, Options, PublishOptions, Publication, Reply, Session, Token};

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A session that lasted this long starts the backoff over.
const BACKOFF_RESET: Duration = Duration::from_secs(10);
/// After a relay of another protocol major: it will not change soon.
const RETRY_INCOMPATIBLE: Duration = Duration::from_secs(10);
/// Frames queued per [`Reader`]; beyond that, the session's own queue
/// fills and drops.
const READER_QUEUE: usize = 64;

/// The connection's state.
#[derive(Clone)]
pub enum Conn {
    Connecting,
    Connected(Session),
    /// Dropped or failed to connect; trying again in `retry_in`.
    Down {
        reason: String,
        retry_in: Duration,
        /// The relay speaks another protocol major.
        incompatible: bool,
    },
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Conn::Connecting => f.write_str("Connecting"),
            Conn::Connected(s) => write!(f, "Connected({})", s.id()),
            Conn::Down { reason, retry_in, .. } => write!(f, "Down({reason}, retry in {retry_in:?})"),
        }
    }
}

type Connect = dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Options>> + Send>> + Send + Sync;

struct Shared {
    connect: Box<Connect>,
    conn: watch::Sender<Conn>,
    room: watch::Sender<Arc<RoomState>>,
    events: broadcast::Sender<RoomEvent>,
    control: broadcast::Sender<Control>,
    replies: broadcast::Sender<Reply>,
    pubs: Mutex<Vec<(Arc<Publication>, PublishOptions)>>,
}

/// Stops the connection when the last [`Client`] clone goes.
struct Run {
    task: tokio::task::JoinHandle<()>,
    shared: Arc<Shared>,
}

impl Run {
    fn stop(&self) {
        self.task.abort();
        let previous = self.shared.conn.send_replace(Conn::Down {
            reason: "closed by the app".into(),
            retry_in: Duration::MAX,
            incompatible: false,
        });
        if let Conn::Connected(s) = previous {
            s.close();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A relay connection that reconnects. Cheap to clone; every clone is the
/// same connection, which ends when the last one is dropped (or on
/// [`Client::close`]).
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    run: Arc<Run>,
}

impl Client {
    /// Starts connecting. `connect` makes the options for each attempt, so
    /// it can discover the relay and fetch a fresh token every time (see
    /// [`Options::discover`]). Must be called in a tokio runtime.
    pub fn new<F, Fut>(connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<Options>> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            connect: Box::new(move || Box::pin(connect())),
            conn: watch::channel(Conn::Connecting).0,
            room: watch::channel(Arc::new(RoomState::default())).0,
            events: broadcast::channel(256).0,
            control: broadcast::channel(256).0,
            replies: broadcast::channel(1024).0,
            pubs: Mutex::default(),
        });
        let run = Run {
            task: tokio::spawn(run(shared.clone())),
            shared: shared.clone(),
        };
        Self {
            shared,
            run: Arc::new(run),
        }
    }

    /// A client for the relay whose web address is `http`
    /// (`http://localhost:8080`): its QUIC endpoint, trust and token found
    /// again on every attempt (see [`Options::discover`]).
    pub fn discover(http: &str, room: &str, token: Token) -> anyhow::Result<Self> {
        let http: url::Url = http.parse()?;
        let room = room.to_string();
        Ok(Self::new(move || {
            let (http, room, token) = (http.clone(), room.clone(), token.clone());
            async move { Options::discover(&http, None, &room, &token, tq_wire::build_info!("tq")).await }
        }))
    }

    pub fn conn(&self) -> watch::Receiver<Conn> {
        self.shared.conn.subscribe()
    }

    /// The current session, while connected.
    pub fn session(&self) -> Option<Session> {
        match &*self.shared.conn.borrow() {
            Conn::Connected(s) if !s.is_closed() => Some(s.clone()),
            _ => None,
        }
    }

    /// The next connected session (the current one, if connected).
    pub async fn connected(&self) -> Session {
        match connected(&mut self.conn()).await {
            Some(s) => s,
            // Closed: nothing will connect again.
            None => std::future::pending().await,
        }
    }

    /// The room's state from the current session; while down, the last
    /// one seen (see [`Client::conn`] for whether it is current).
    pub fn room(&self) -> watch::Receiver<Arc<RoomState>> {
        self.shared.room.subscribe()
    }

    /// The room's events, from every session in turn.
    pub fn events(&self) -> broadcast::Receiver<RoomEvent> {
        self.shared.events.subscribe()
    }

    /// Control messages from the relay, from every session in turn.
    pub fn control(&self) -> broadcast::Receiver<Control> {
        self.shared.control.subscribe()
    }

    /// Acks and echoes, from every session in turn.
    pub fn replies(&self) -> broadcast::Receiver<Reply> {
        self.shared.replies.subscribe()
    }

    /// Subscribes to a track on every session from now on. Group numbers
    /// keep increasing across sessions, so a reconnect looks like a new
    /// group (a new GOP, for video) to the reader.
    pub fn subscribe(&self, sub: impl Into<Subscription>) -> Reader {
        let sub = sub.into();
        let path = sub.track.clone();
        let (tx, rx) = mpsc::channel(READER_QUEUE);
        let task = tokio::spawn(resubscribe(self.conn(), sub, tx));
        Reader { path, rx, task }
    }

    /// Publishes `p` on the current session and every one after it.
    pub fn publish(&self, p: &Arc<Publication>, opts: PublishOptions) {
        // Under the lock: `serve` publishes what was added before it
        // connected, and this what was added after.
        let session = {
            let mut pubs = self.shared.pubs.lock().unwrap();
            pubs.push((p.clone(), opts.clone()));
            self.session()
        };
        if let Some(session) = session {
            let p = p.clone();
            tokio::spawn(async move {
                if let Err(e) = session.publish_with(&p, opts).await {
                    tracing::warn!(broadcast = %p.name(), error = format!("{e:#}"), "publish failed");
                }
            });
        }
    }

    /// Closes the current session and stops reconnecting. Readers end.
    pub fn close(&self) {
        self.run.stop();
    }
}

/// The next live session; `None` once the client is closed or gone.
async fn connected(conn: &mut watch::Receiver<Conn>) -> Option<Session> {
    let c = conn
        .wait_for(|c| match c {
            Conn::Connected(s) => !s.is_closed(),
            Conn::Down { retry_in, .. } => *retry_in == Duration::MAX,
            Conn::Connecting => false,
        })
        .await;
    match c.as_deref() {
        Ok(Conn::Connected(s)) => Some(s.clone()),
        _ => None,
    }
}

async fn run(shared: Arc<Shared>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        shared.conn.send_replace(Conn::Connecting);
        let result = async { Session::connect((shared.connect)().await?).await }.await;
        let (reason, wait, incompatible) = match result {
            Ok(session) => {
                let started = Instant::now();
                let reason = serve(&shared, &session).await;
                session.close();
                if started.elapsed() >= BACKOFF_RESET {
                    backoff = BACKOFF_MIN;
                }
                (reason, backoff, false)
            }
            Err(e) if e.is::<Incompatible>() => (format!("{e:#}"), RETRY_INCOMPATIBLE, true),
            Err(e) => (format!("{e:#}"), backoff, false),
        };
        tracing::debug!(%reason, retry_in = ?wait, "relay session down");
        shared.conn.send_replace(Conn::Down {
            reason,
            retry_in: wait,
            incompatible,
        });
        tokio::time::sleep(wait).await;
        if !incompatible {
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// One session: publishes, then passes its state, events, control
/// messages and replies on until it ends. Returns why it ended.
async fn serve(shared: &Shared, session: &Session) -> String {
    // Taken first, so nothing that follows the welcome is missed.
    let (mut room, mut events, mut control, mut replies) =
        (session.room(), session.events(), session.control(), session.replies());
    let publish = async |pubs: Vec<(Arc<Publication>, PublishOptions)>| {
        for (p, opts) in pubs {
            if let Err(e) = session.publish_with(&p, opts).await {
                tracing::warn!(broadcast = %p.name(), error = format!("{e:#}"), "publish failed");
            }
        }
    };
    let pubs = shared.pubs.lock().unwrap().clone();
    let published = pubs.len();
    publish(pubs).await;
    // Those added meanwhile; from now on `Client::publish` does it.
    let added = {
        let pubs = shared.pubs.lock().unwrap();
        shared.conn.send_replace(Conn::Connected(session.clone()));
        pubs[published..].to_vec()
    };
    publish(added).await;
    // A new session's room is empty until its first state: passing that
    // on would make it look as if everything had gone.
    let pass_room = |state: Arc<RoomState>| {
        if !state.room_id.is_empty() {
            shared.room.send_replace(state);
        }
    };
    pass_room(room.borrow_and_update().clone());
    fn pass<T: Clone>(r: Result<T, RecvError>, tx: &broadcast::Sender<T>) -> bool {
        match r {
            Ok(m) => {
                let _ = tx.send(m);
                true
            }
            Err(RecvError::Lagged(_)) => true,
            Err(RecvError::Closed) => false,
        }
    }
    loop {
        let open = tokio::select! {
            r = room.changed() => {
                let open = r.is_ok();
                if open {
                    pass_room(room.borrow_and_update().clone());
                }
                open
            }
            r = events.recv() => pass(r, &shared.events),
            r = control.recv() => pass(r, &shared.control),
            r = replies.recv() => pass(r, &shared.replies),
            why = session.closed() => return why,
        };
        if !open {
            return session.closed().await;
        }
    }
}

/// Subscribes on each session in turn, forwarding frames with group
/// numbers that keep increasing.
async fn resubscribe(mut conn: watch::Receiver<Conn>, sub: Subscription, tx: mpsc::Sender<Frame>) {
    let mut next_base = 0u64;
    loop {
        let session = tokio::select! {
            s = connected(&mut conn) => match s {
                Some(s) => s,
                None => return,
            },
            _ = tx.closed() => return,
        };
        match session.subscribe(sub.clone()).await {
            Ok(mut reader) => {
                let base = next_base;
                loop {
                    let f = tokio::select! {
                        f = reader.recv() => f,
                        _ = tx.closed() => return,
                    };
                    let Some(mut f) = f else { break };
                    // Datagram tracks have no groups.
                    if f.group != 0 {
                        f.group += base;
                        next_base = next_base.max(f.group + 1);
                    }
                    if tx.send(f).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(track = %sub.track, error = format!("{e:#}"), "subscribe failed");
                // Tried again on the next session.
                tokio::select! {
                    _ = session.closed() => {}
                    _ = tx.closed() => return,
                }
            }
        }
    }
}

/// Frames of a track across sessions. Dropping it unsubscribes.
pub struct Reader {
    path: Path,
    rx: mpsc::Receiver<Frame>,
    task: tokio::task::JoinHandle<()>,
}

impl Reader {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The next frame; `None` only once the client is gone.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Frame> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.task.abort();
    }
}
