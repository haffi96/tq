//! What the panels show: connection and room state, the stats windows,
//! the operator's keys and the event log. An entity of its own so the
//! panels re-render when it changes (control messages, room events, key
//! presses and the stats tick), never for a video frame.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};

use gpui_kit::Context;
use tq_proto::{
    BuildInfo, Compat, Control, Event as RoomEvent, OperatorChange, PublisherGone, Role, RoomMessage, RoomState,
};

use crate::{
    feed::Stats,
    session::{Command, ConnState, Drive, Event, Shared},
    stats::Window as Win,
    update,
};

const LOG_LINES: usize = 200;
/// Stats ticks kept for the sparklines: 60 s at 2 Hz.
const HISTORY: usize = 120;

pub struct Model {
    pub shared: Arc<Shared>,
    pub conn: (ConnState, Option<String>),
    pub session: Option<String>,
    pub state: Option<RoomState>,
    pub link: Option<String>,
    pub stats: Vec<(String, Stats)>,
    /// Teleop ack and heartbeat round trips over the last stats interval.
    pub teleop_rtt: (Win, Win),
    /// Render start -> last element painted, over the last stats interval.
    pub frame_cost: Win,
    pub keys: BTreeSet<&'static str>,
    pub streaming: bool,
    /// Which dock panels are open.
    pub sidebar: bool,
    pub bottom: bool,
    pub log: Vec<(String, String)>,
    /// Per camera capture -> render, oldest first; `None` for a tick
    /// without frames.
    pub history: HashMap<String, VecDeque<Option<f64>>>,
    /// Publisher round trip, oldest first.
    pub rtt_history: VecDeque<Option<f64>>,
    /// The relay's build from its welcome; `None` before 1.1 or before
    /// connecting.
    pub relay_build: Option<BuildInfo>,
    /// The last publisher build logged, to log changes once.
    publisher_seen: Option<String>,
    pub updates: update::Settings,
    pub update: UpdateState,
    ticks: u64,
}

#[derive(Debug, Clone)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(update::Release),
    Installing,
    Failed(String),
}

impl Model {
    pub fn new(shared: Arc<Shared>, updates: update::Settings) -> Self {
        Self {
            shared,
            conn: (ConnState::Connecting, None),
            session: None,
            state: None,
            link: None,
            stats: Vec::new(),
            teleop_rtt: Default::default(),
            frame_cost: Default::default(),
            keys: BTreeSet::new(),
            streaming: false,
            sidebar: true,
            bottom: true,
            log: Vec::new(),
            history: HashMap::new(),
            rtt_history: VecDeque::new(),
            relay_build: None,
            publisher_seen: None,
            updates,
            update: UpdateState::Idle,
            ticks: 0,
        }
    }

    /// This session holds the operator claim.
    pub fn driving(&self) -> bool {
        let holder = self.state.as_ref().and_then(|s| s.operator.as_ref());
        holder.is_some() && holder == self.session.as_ref()
    }

    pub fn publisher_build(&self) -> Option<&BuildInfo> {
        self.state.as_ref()?.publisher.as_ref()?.build.as_ref()
    }

    /// Logs the publisher's build when it first shows up or changes.
    fn note_publisher(&mut self) {
        let Some(p) = self.state.as_ref().and_then(|s| s.publisher.as_ref()) else {
            return;
        };
        let line = match &p.build {
            Some(b) => format!("Publisher {} runs {}", p.publisher_id, b.long()),
            None => format!("Publisher {} runs an unknown version (protocol 1.0)", p.publisher_id),
        };
        if self.publisher_seen.as_ref() == Some(&line) {
            return;
        }
        self.publisher_seen = Some(line.clone());
        let build = p.build.clone();
        self.push_log(line);
        if Compat::of_build(build.as_ref()) != Compat::Same {
            self.push_log(format!(
                "Warning: {}",
                tq_proto::mismatch("publisher", build.as_ref(), "tq-app")
            ));
        }
    }

    // --- updates --------------------------------------------------------

    pub fn check_update(&mut self, cx: &mut Context<Self>) {
        if matches!(self.update, UpdateState::Checking | UpdateState::Installing) {
            return;
        }
        self.update = UpdateState::Checking;
        cx.notify();
        let settings = self.updates.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { update::check(&settings) })
                .await;
            let _ = this.update(cx, |m, cx| {
                m.update = match result {
                    Ok(Some(release)) => {
                        m.push_log(format!("Update available: tq-app {}", release.version));
                        UpdateState::Available(release)
                    }
                    Ok(None) => UpdateState::UpToDate,
                    Err(e) => {
                        tracing::warn!("update check: {e:#}");
                        UpdateState::Failed(format!("{e:#}"))
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Downloads and swaps in the available release, then restarts. Refused
    /// while driving: the restart drops the session, and the robot would
    /// trip its dead-man mid-drive.
    pub fn install_update(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(release) = self.update.clone() else {
            return;
        };
        if self.driving() {
            self.push_log("Release the claim before updating".into());
            cx.notify();
            return;
        }
        self.update = UpdateState::Installing;
        self.push_log(format!("Installing tq-app {}…", release.version));
        cx.notify();
        let settings = self.updates.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let installed = update::install(&settings, &release, &|s| tracing::info!("update: {s}"))?;
                    update::relaunch(&installed)?;
                    anyhow::Ok(())
                })
                .await;
            let _ = this.update(cx, |m, cx| match result {
                Ok(()) => cx.quit(),
                Err(e) => {
                    let e = format!("{e:#}");
                    m.push_log(format!("Update failed: {e}"));
                    m.update = UpdateState::Failed(e);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn operator(&self) -> bool {
        self.shared.config.role == Role::Operator
    }

    /// The stats tick: takes every window the panels and tiles show.
    pub fn tick(&mut self, frame_cost: Win, cx: &mut Context<Self>) {
        self.stats = self
            .shared
            .feeds()
            .iter()
            .map(|f| (f.info.id.clone(), f.take_stats()))
            .collect();
        self.frame_cost = frame_cost;
        self.ticks += 1;
        if self.shared.config.log_stats && self.ticks.is_multiple_of(4) {
            let sync = self.shared.clock.lock().unwrap().sync_error_ms();
            for (id, s) in &self.stats {
                tracing::info!("{id}: {}", super::tile::overlay_text(s, sync).replace('\n', " | "));
            }
            tracing::info!("ui frame cost ms avg/max {}", self.frame_cost);
        }
        let mut t = self.shared.teleop.lock().unwrap();
        self.teleop_rtt = (t.ack_rtt.take(), t.heartbeat_rtt.take());
        drop(t);
        for (id, s) in &self.stats {
            let h = self.history.entry(id.clone()).or_default();
            push(h, s.capture_to_render.avg);
        }
        let rtt = self
            .state
            .as_ref()
            .and_then(|s| s.publisher.as_ref())
            .and_then(|p| p.rtt_ms);
        push(&mut self.rtt_history, rtt);
        cx.notify();
    }

    /// Everything but [`Event::Frame`], which goes to the tiles.
    pub fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Frame => return,
            Event::Status(state, detail) => {
                if state != ConnState::Connected {
                    // The next session's events stream starts with a state.
                    self.session = None;
                    self.state = None;
                }
                if let (ConnState::Closed, Some(d)) = (state, &detail) {
                    self.push_log(format!("Disconnected: {d}"));
                }
                if state == ConnState::Connected && self.conn.0 != ConnState::Connected {
                    self.push_log("Connected to the relay".into());
                }
                self.conn = (state, detail);
            }
            Event::Room(RoomMessage::State { state, .. }) => {
                self.state = Some(state);
                self.note_publisher();
            }
            Event::Room(RoomMessage::Event(e)) => {
                if let Some(line) = self.describe(&e.event) {
                    self.push_log(line);
                }
            }
            Event::Room(RoomMessage::Unknown) => {}
            Event::Control(msg) => match msg {
                Control::Welcome { session, relay, .. } => {
                    self.session = Some(session);
                    self.push_log(match &relay {
                        Some(b) => format!("Relay runs {}", b.long()),
                        None => "Relay runs an unknown version (protocol 1.0)".into(),
                    });
                    if Compat::of_build(relay.as_ref()) != Compat::Same {
                        self.push_log(format!(
                            "Warning: {}",
                            tq_proto::mismatch("relay", relay.as_ref(), "tq-app")
                        ));
                    }
                    self.relay_build = relay;
                }
                Control::ClaimResult { ok: false, reason, .. } => {
                    self.push_log(format!("Claim refused: {}", reason.unwrap_or_default()))
                }
                // The relay answers a release with a result too.
                Control::ClaimResult { ok: true, holder, .. } if holder.is_some() && holder == self.session => {
                    self.push_log("Claim granted: you are driving".into())
                }
                Control::LinkStats { quic } => {
                    self.link = Some(format!(
                        "downlink quic: rtt {:.1} ms  cwnd {} KiB  lost {}/{}",
                        quic.rtt_ms,
                        quic.cwnd / 1024,
                        quic.lost_packets,
                        quic.sent_packets
                    ))
                }
                Control::Error { message } => self.push_log(format!("Relay error: {message}")),
                _ => {}
            },
        }
        cx.notify();
    }

    /// A session, as the log shows it: who and which session, or "you".
    fn who(&self, session: &str) -> String {
        if self.session.as_deref() == Some(session) {
            return "you".into();
        }
        let short = &session[..session.len().min(8)];
        let sub = self
            .state
            .as_ref()
            .and_then(|s| s.participants.iter().find(|p| p.session == session))
            .map(|p| p.sub.clone());
        match sub {
            Some(sub) => format!("{sub} ({short})"),
            None => short.to_string(),
        }
    }

    /// A room event as a log line; `None` for those not worth one.
    fn describe(&self, event: &RoomEvent) -> Option<String> {
        Some(match event {
            RoomEvent::ParticipantJoined { participant: p } if self.session.as_ref() != Some(&p.session) => {
                format!("{} joined as {}", self.who_info(&p.sub, &p.session), p.role)
            }
            RoomEvent::ParticipantJoined { .. } => return None,
            RoomEvent::ParticipantLeft { participant: p } => {
                format!("{} ({}) left", self.who_info(&p.sub, &p.session), p.role)
            }
            RoomEvent::PublisherConnected { publisher_id, .. } => format!("Publisher {publisher_id} connected"),
            RoomEvent::PublisherDisconnected { publisher_id, reason } => match reason {
                PublisherGone::Disconnected => format!("Publisher {publisher_id} disconnected"),
                PublisherGone::Replaced => format!("Publisher {publisher_id} replaced by a new session"),
            },
            RoomEvent::TrackPublished { track } => format!("Camera {} published", track.camera.id),
            RoomEvent::TrackUpdated { track } => format!(
                "Camera {} changed: {} {}x{}",
                track.camera.id, track.camera.codec, track.camera.width, track.camera.height
            ),
            RoomEvent::TrackUnpublished { camera, reason } => format!("Camera {camera} unpublished: {reason}"),
            RoomEvent::TrackOnline { camera } => format!("Camera {camera} online"),
            RoomEvent::TrackOffline { camera, reason } => format!("Camera {camera} offline: {reason}"),
            RoomEvent::OperatorChanged { from, to, reason } => match (reason, from, to) {
                (OperatorChange::Claimed, _, Some(to)) => format!("{} took the claim", self.who(to)),
                (OperatorChange::Forced, Some(from), Some(to)) => {
                    format!("{} took the claim from {}", self.who(to), self.who(from))
                }
                (OperatorChange::Released, Some(from), _) => format!("{} released the claim", self.who(from)),
                (OperatorChange::Left, Some(from), _) => format!("Claim freed: {} left", self.who(from)),
                _ => "Operator changed".into(),
            },
            RoomEvent::DeadmanTripped { reason } => format!("Robot safe-stopped: {reason}"),
            RoomEvent::DeadmanCleared => "Robot armed".into(),
            RoomEvent::Unknown => return None,
        })
    }

    fn who_info(&self, sub: &str, session: &str) -> String {
        format!("{sub} ({})", &session[..session.len().min(8)])
    }

    fn push_log(&mut self, line: String) {
        self.log.insert(0, (time_of_day(), line));
        self.log.truncate(LOG_LINES);
    }

    pub fn control(&self, msg: Control) {
        self.shared.send(Command::Control(msg));
    }

    // --- teleop ---------------------------------------------------------

    pub fn update_drive(&mut self, cx: &mut Context<Self>) {
        let k = |c| self.keys.contains(c) as i32 as f64;
        let drive = Drive {
            throttle: k("w") - k("s"),
            steer: k("d") - k("a"),
            active: !self.keys.is_empty() || self.streaming,
        };
        *self.shared.drive.lock().unwrap() = drive;
        cx.notify();
    }

    /// Emergency stop: drop the claim; the robot trips at once.
    pub fn estop(&mut self, cx: &mut Context<Self>) {
        self.keys.clear();
        self.streaming = false;
        self.update_drive(cx);
        self.control(Control::Release);
        self.push_log("E-stop: claim released".into());
    }
}

fn push<T>(h: &mut VecDeque<T>, v: T) {
    if h.len() == HISTORY {
        h.pop_front();
    }
    h.push_back(v);
}

fn time_of_day() -> String {
    let secs = tq_proto::now_us() / 1_000_000;
    format!("{:02}:{:02}:{:02}Z", secs / 3600 % 24, secs / 60 % 60, secs % 60)
}
