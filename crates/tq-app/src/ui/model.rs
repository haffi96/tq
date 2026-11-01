//! What the panels show: connection and room state, the stats windows,
//! the operator's keys and the event log. An entity of its own so the
//! panels re-render when it changes (room states and events, key presses
//! and the stats tick), never for a video frame.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};

use gpui_kit::Context;
use tq_wire::{
    BroadcastState, BuildInfo, Compat, Control, Event as RoomEvent, LockChange, Perms, RoomState, StopReason,
};

use crate::{
    feed::Stats,
    layout::{self, Layout},
    session::{self, ConnState, Drive, Event, Shared},
    stats::Window as Win,
    update,
};

const LOG_LINES: usize = 200;
/// Stats ticks kept for the sparklines: 60 s at 2 Hz.
const HISTORY: usize = 120;

/// Teleop's state as the robot reports it on its `.status` track.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RobotTeleop {
    pub tripped: bool,
    pub reason: String,
    pub holder: Option<String>,
}

pub struct Model {
    pub shared: Arc<Shared>,
    pub conn: (ConnState, Option<String>),
    /// This session's id, while connected.
    pub session: Option<String>,
    pub state: Option<Arc<RoomState>>,
    pub link: Option<String>,
    /// Per feed (by track path).
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
    /// Newest first: when it happened (Unix µs, this machine's clock) and
    /// the line.
    pub log: Vec<(u64, String)>,
    /// Per feed capture -> render, oldest first; `None` for a tick
    /// without frames.
    pub history: HashMap<String, VecDeque<Option<f64>>>,
    /// Round trip to the owner of the broadcast the link panel shows,
    /// oldest first.
    pub rtt_history: VecDeque<Option<f64>>,
    /// The relay's build from its welcome.
    pub relay_build: Option<BuildInfo>,
    /// Each broadcast's newest `.status`.
    pub statuses: HashMap<String, serde_json::Value>,
    /// The last owner build logged per broadcast, to log changes once.
    owners_seen: HashMap<String, String>,
    pub updates: update::Settings,
    pub update: UpdateState,
    /// What the board shows: the user's layout (given, or edited here), or
    /// the preset made from the room.
    pub layout: Arc<Layout>,
    /// `layout` is the user's: never replaced by the preset.
    pub layout_saved: bool,
    /// Where edits are saved; `None` without a settings directory (they
    /// last until the app quits).
    pub store: Option<layout::Store>,
    /// The editor is open.
    pub editing: bool,
    /// The widget the editor shows.
    pub selected: Option<String>,
    /// A session's permissions have let it write a room's stop channel.
    stop_permitted: bool,
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

/// What a participant's permissions make it, for counts and the log.
pub fn kind_of(perms: &Perms) -> &'static str {
    if perms.can_publish() {
        "robot"
    } else if perms
        .0
        .iter()
        .any(|p| p.starts_with("lock:") || p.starts_with("force:") || p.starts_with("write:"))
    {
        "operator"
    } else {
        "viewer"
    }
}

impl Model {
    pub fn new(
        shared: Arc<Shared>,
        updates: update::Settings,
        layout: Option<Layout>,
        store: Option<layout::Store>,
    ) -> Self {
        let layout_saved = layout.is_some();
        Self {
            layout: Arc::new(layout.unwrap_or_else(|| layout::preset(&RoomState::default(), false))),
            layout_saved,
            store,
            editing: false,
            selected: None,
            stop_permitted: false,
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
            // The board has latency and status; the sidebar is one click away.
            sidebar: false,
            // The preset has a log widget; the panel has the tracks table.
            bottom: false,
            log: Vec::new(),
            history: HashMap::new(),
            rtt_history: VecDeque::new(),
            relay_build: None,
            statuses: HashMap::new(),
            owners_seen: HashMap::new(),
            updates,
            update: UpdateState::Idle,
            ticks: 0,
        }
    }

    /// The broadcast the operator drives: the first with a `drive` lock.
    pub fn drive_broadcast(&self) -> Option<&BroadcastState> {
        session::drive_broadcast(self.state.as_deref()?)
    }

    /// The broadcast the link panel and title bar show: the driven one,
    /// else the first.
    pub fn main_broadcast(&self) -> Option<&BroadcastState> {
        self.drive_broadcast()
            .or_else(|| self.state.as_ref()?.broadcasts.first())
    }

    /// This app holds a lock (an update's restart would drop it).
    pub fn driving(&self) -> bool {
        self.shared.holds_any()
    }

    /// Teleop's state on a broadcast, from its `.status`.
    pub fn robot_teleop(&self, broadcast: &str) -> Option<RobotTeleop> {
        let t = self.statuses.get(broadcast)?.get("teleop")?;
        Some(RobotTeleop {
            tripped: t.get("tripped")?.as_bool()?,
            reason: t.get("reason").and_then(|r| r.as_str()).unwrap_or_default().to_string(),
            holder: t.get("holder").and_then(|h| h.as_str()).map(str::to_string),
        })
    }

    /// Logs each broadcast owner's build when it first shows up or changes.
    fn note_owners(&mut self) {
        let Some(state) = self.state.clone() else {
            return;
        };
        for b in state.broadcasts.iter().filter(|b| b.owner.is_some()) {
            let line = match &b.build {
                Some(build) => format!("Broadcast {} runs {}", b.name, build.long()),
                None => format!("Broadcast {} runs an unknown version", b.name),
            };
            if self.owners_seen.get(&b.name) == Some(&line) {
                continue;
            }
            self.owners_seen.insert(b.name.clone(), line.clone());
            self.push_log(line);
            if Compat::of_build(b.build.as_ref()) != Compat::Same {
                self.push_log(format!(
                    "Warning: {}",
                    tq_wire::mismatch(&format!("broadcast {}", b.name), b.build.as_ref(), "tq-app")
                ));
            }
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
        self.shared.config.operator()
    }

    /// Whether the app offers the remote stop (button and Space): for the
    /// operator preset always, and for any session whose permissions let
    /// it write a stop channel in the room, whatever the preset. Once
    /// offered it stays, so a robot that drops out and comes back can be
    /// stopped just the same.
    pub fn can_stop(&self) -> bool {
        self.operator() || self.stop_permitted
    }

    /// The stats tick: takes every window the panels and tiles show.
    pub fn tick(&mut self, frame_cost: Win, cx: &mut Context<Self>) {
        let feeds = self.shared.feeds();
        self.stats = feeds.iter().map(|f| (f.path.to_string(), f.take_stats())).collect();
        self.frame_cost = frame_cost;
        self.ticks += 1;
        if self.shared.config.log_stats && self.ticks.is_multiple_of(4) {
            for (f, (id, s)) in feeds.iter().zip(&self.stats) {
                let sync = self.shared.sync_error_ms(&f.path.broadcast);
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
        let rtt = self.main_broadcast().and_then(|b| b.rtt_ms);
        push(&mut self.rtt_history, rtt);
        cx.notify();
    }

    /// Everything but [`Event::Frame`], which goes to the tiles.
    pub fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Frame | Event::Data => return,
            Event::Status(state, detail) => {
                if state != ConnState::Connected {
                    // The next session starts with a fresh state.
                    self.session = None;
                    self.state = None;
                    self.statuses.clear();
                }
                if let (ConnState::Closed, Some(d)) = (state, &detail) {
                    self.push_log(format!("Disconnected: {d}"));
                }
                if state == ConnState::Connected && self.conn.0 != ConnState::Connected {
                    self.push_log("Connected to the relay".into());
                }
                self.conn = (state, detail);
            }
            Event::Connected { session, relay } => {
                self.session = Some(session);
                self.push_log(format!("Relay runs {}", relay.long()));
                if Compat::of_build(Some(&relay)) != Compat::Same {
                    self.push_log(format!(
                        "Warning: {}",
                        tq_wire::mismatch("relay", Some(&relay), "tq-app")
                    ));
                }
                self.relay_build = Some(relay);
            }
            Event::Room(state) => {
                // Statuses of broadcasts that went away go with them.
                self.statuses.retain(|b, _| state.broadcast(b).is_some());
                if !self.layout_saved {
                    let preset = layout::preset(&state, self.operator());
                    if preset != *self.layout {
                        self.layout = Arc::new(preset);
                    }
                }
                if let Some(perms) = self.shared.perms() {
                    self.stop_permitted |= state.broadcasts.iter().any(|b| {
                        b.channel(session::STOP).is_some()
                            && perms.allows(tq_wire::Verb::Write, &tq_wire::Path::new(&b.name, session::STOP))
                    });
                }
                // Streaming needs a lock to drive with: losing every lock
                // turns it off, so a later claim does not resume it.
                if self.streaming && !self.shared.holds_any() {
                    self.streaming = false;
                    self.update_drive(cx);
                }
                self.state = Some(state);
                self.note_owners();
            }
            Event::RoomEvent(e) => {
                if let Some(line) = self.describe(&e.event) {
                    self.push_log(line);
                }
            }
            Event::BroadcastStatus(broadcast, status) => {
                let before = self.robot_teleop(&broadcast);
                self.statuses.insert(broadcast.clone(), status);
                let after = self.robot_teleop(&broadcast);
                match (before.map(|t| t.tripped), after) {
                    (Some(false), Some(t)) if t.tripped => {
                        self.push_log(format!("{broadcast} safe-stopped: {}", t.reason))
                    }
                    (Some(true), Some(t)) if !t.tripped => self.push_log(format!("{broadcast} armed")),
                    _ => {}
                }
            }
            Event::Log(line) => self.push_log(line),
            Event::LogAt(at_us, line) => self.push_log_at(at_us, line),
            Event::Control(msg) => match msg {
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
                format!("{} joined as {}", self.who_info(&p.sub, &p.session), kind_of(&p.perms))
            }
            RoomEvent::ParticipantJoined { .. } => return None,
            RoomEvent::ParticipantLeft { participant: p } => {
                format!("{} ({}) left", self.who_info(&p.sub, &p.session), kind_of(&p.perms))
            }
            RoomEvent::BroadcastStarted { broadcast, owner, .. } => {
                format!("Broadcast {broadcast} started by {}", self.who(owner))
            }
            RoomEvent::BroadcastStopped { broadcast, reason } => match reason {
                StopReason::Disconnected => format!("Broadcast {broadcast} stopped: owner disconnected"),
                StopReason::Replaced => format!("Broadcast {broadcast} taken over by a new session"),
                StopReason::Unadvertised => format!("Broadcast {broadcast} unadvertised"),
            },
            RoomEvent::BroadcastRemoved { broadcast } => format!("Broadcast {broadcast} removed"),
            RoomEvent::TrackPublished { track, info } => match &info.video {
                Some(v) => format!("Track {track} published: video {}", v.codec),
                None => format!("Track {track} published"),
            },
            RoomEvent::TrackUpdated { track, info } => match &info.video {
                Some(v) => format!("Track {track} changed: {} {}x{}", v.codec, v.width, v.height),
                None => format!("Track {track} changed"),
            },
            RoomEvent::TrackUnpublished { track } => format!("Track {track} unpublished"),
            RoomEvent::TrackOnline { track } => format!("Track {track} online"),
            RoomEvent::TrackOffline { track, reason } if reason.is_empty() => format!("Track {track} offline"),
            RoomEvent::TrackOffline { track, reason } => format!("Track {track} offline: {reason}"),
            RoomEvent::LockChanged {
                lock, from, to, reason, ..
            } => match (reason, from, to) {
                (LockChange::Claimed, _, Some(to)) => format!("{} took {lock}", self.who(to)),
                (LockChange::Forced, Some(from), Some(to)) => {
                    format!("{} took {lock} from {}", self.who(to), self.who(from))
                }
                (LockChange::Released, Some(from), _) => format!("{} released {lock}", self.who(from)),
                (LockChange::Left, Some(from), _) => format!("{lock} freed: {} left", self.who(from)),
                _ => format!("{lock} changed hands"),
            },
            RoomEvent::Unknown => return None,
        })
    }

    fn who_info(&self, sub: &str, session: &str) -> String {
        format!("{sub} ({})", &session[..session.len().min(8)])
    }

    fn push_log(&mut self, line: String) {
        self.push_log_at(tq_wire::now_us(), line);
    }

    /// A line that happened at `at_us`, in time order: a broadcast's
    /// `.events` replayed on subscribing can be older than lines before it.
    fn push_log_at(&mut self, at_us: u64, line: String) {
        let i = self.log.partition_point(|(t, _)| *t > at_us);
        self.log.insert(i, (at_us, line));
        self.log.truncate(LOG_LINES);
    }

    // --- layout editing -------------------------------------------------

    /// Changes the layout and saves it: from now on it is the user's, and
    /// the room no longer remakes it.
    pub fn edit_layout(&mut self, f: impl FnOnce(&mut Layout), cx: &mut Context<Self>) {
        let mut layout = (*self.layout).clone();
        f(&mut layout);
        self.set_layout(layout, cx);
    }

    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|id| !layout.widgets.iter().any(|w| &w.id == id))
        {
            self.selected = None;
        }
        self.layout = Arc::new(layout);
        self.layout_saved = true;
        if let Some(store) = &self.store {
            if let Err(e) = store.save(&self.layout) {
                self.push_log(format!("Layout not saved to {}: {e}", store.path.display()));
            }
        }
        cx.notify();
    }

    /// Back to the preset made from the room. The saved layout is
    /// forgotten; a `--layout` file stays as it is, and is written again on
    /// the next edit.
    pub fn reset_layout(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = &self.store {
            if let Err(e) = store.forget() {
                self.push_log(format!("Saved layout not removed: {e}"));
            }
        }
        self.layout_saved = false;
        self.selected = None;
        let room = self.state.clone().unwrap_or_default();
        self.layout = Arc::new(layout::preset(&room, self.operator()));
        cx.notify();
    }

    /// Takes a layout pasted as JSON, or says why not.
    pub fn import_layout(&mut self, json: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let layout = Layout::parse(json).map_err(|e| e.join("\n"))?;
        self.set_layout(layout, cx);
        self.push_log("Layout imported".into());
        Ok(())
    }

    /// Where the layout is, for the editor.
    pub fn layout_place(&self) -> String {
        match (&self.store, self.layout_saved) {
            (_, false) => "preset (not saved)".into(),
            (Some(s), true) => format!("saved to {}", s.path.display()),
            (None, true) => "not saved (no settings directory)".into(),
        }
    }

    pub fn set_editing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.editing = on;
        if !on {
            self.selected = None;
        }
        cx.notify();
    }

    pub fn select(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected != id {
            self.selected = id;
            cx.notify();
        }
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

    /// Emergency stop: every robot in the room safe-stops, whoever drives
    /// it, until its drive lock is claimed again. Stops sending, and lets
    /// go of this app's own drive lock.
    pub fn remote_stop(&mut self, cx: &mut Context<Self>) {
        self.keys.clear();
        self.streaming = false;
        self.update_drive(cx);
        self.shared.remote_stop();
        self.push_log("Remote stop sent".into());
    }
}

fn push<T>(h: &mut VecDeque<T>, v: T) {
    if h.len() == HISTORY {
        h.pop_front();
    }
    h.push_back(v);
}

pub fn time_of_day(us: u64) -> String {
    let secs = us / 1_000_000;
    format!("{:02}:{:02}:{:02}Z", secs / 3600 % 24, secs / 60 % 60, secs % 60)
}
