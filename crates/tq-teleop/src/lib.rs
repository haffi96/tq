//! Teleoperation, built from tq's general parts:
//!
//! - a `drive` channel behind a `drive` lock, so one operator drives at a
//!   time, with a watchdog on the lock: the dead-man switch. The robot is
//!   safe-stopped unless the lock's holder is alive and sending (see
//!   [`tq::WatchdogStatus`]);
//! - an `estop` channel anyone allowed to write may use;
//! - `.status` and `.events` data tracks the consoles show.
//!
//! The robot side adds these to its broadcast with [`advertise`], then
//! hands drive and e-stop messages to a [`Teleop`], which applies them to a
//! [`Sink`] (the robot's motion interface, which is not part of this repo).
//!
//! Drive messages are JSON: `{"command": "drive", "args": {"throttle":
//! 0.5, "steer": -0.2}}`. Commands are latest-wins; the SDK acks stale
//! sequence numbers as superseded before they get here.

use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tq::{
    wire::{Advertisement, ChannelInfo, DataInfo, Delivery, LockInfo, TrackInfo, Watchdog},
    Message, Publication,
};

/// The drive channel, and the lock that guards it.
pub const DRIVE: &str = "drive";
pub const ESTOP: &str = "estop";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Trip the dead-man when the holder's heartbeats stop for this long.
    pub heartbeat_loss_ms: u64,
    /// Once commands have started, trip when they stop for this long.
    pub command_stale_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat_loss_ms: 500,
            command_stale_ms: 500,
        }
    }
}

/// Adds teleop's channels, lock and status tracks to a broadcast.
pub fn advertise(ad: &mut Advertisement, cfg: &Config) {
    let channel = |id: &str, label: &str, lock: Option<&str>| ChannelInfo {
        id: id.into(),
        label: label.into(),
        lock: lock.map(Into::into),
        encoding: "json".into(),
        schema: None,
    };
    ad.channels.push(channel(DRIVE, "Drive", Some(DRIVE)));
    ad.channels.push(channel(ESTOP, "E-stop", None));
    ad.locks.push(LockInfo {
        id: DRIVE.into(),
        label: "Drive".into(),
        watchdog: Some(Watchdog {
            heartbeat_loss_ms: cfg.heartbeat_loss_ms,
            message_stale_ms: cfg.command_stale_ms,
        }),
    });
    for (id, label, delivery) in [
        (".status", "Status", Delivery::Latest),
        (".events", "Events", Delivery::Stream),
    ] {
        if ad.track(id).is_none() {
            let mut t = TrackInfo::data(
                id,
                DataInfo {
                    encoding: "json".into(),
                    delivery,
                    rate_hz: None,
                    window_ms: None,
                },
            );
            t.label = label.into();
            ad.tracks.push(t);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Command {
    pub command: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Where accepted commands go: the robot's motion interface.
pub trait Sink: Send + Sync {
    fn apply(&self, cmd: &Command) -> Result<(), String>;
    fn safe_stop(&self, reason: &str);
}

/// Logs instead of moving anything.
pub struct LoggingSink;

impl Sink for LoggingSink {
    fn apply(&self, cmd: &Command) -> Result<(), String> {
        tracing::debug!(command = %cmd.command, args = %cmd.args, "teleop apply");
        Ok(())
    }

    fn safe_stop(&self, reason: &str) {
        tracing::warn!(reason, "teleop SAFE STOP");
    }
}

/// Teleop's part of the robot's status.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Status {
    /// Safe-stopped; stays so until the next accepted command.
    pub tripped: bool,
    pub reason: String,
    /// Session holding the drive lock.
    pub holder: Option<String>,
    pub commands: u64,
    pub last_seq: u32,
    pub estops: u64,
}

pub struct Teleop {
    p: Arc<Publication>,
    sink: Arc<dyn Sink>,
    commands: AtomicU64,
    estops: AtomicU64,
    last_seq: Mutex<u32>,
    status: watch::Sender<Status>,
}

impl Teleop {
    /// Follows `p`'s drive lock: its holder, and its watchdog, whose trips
    /// safe-stop the sink. `p` must have been advertised with
    /// [`advertise`].
    pub fn new(p: &Arc<Publication>, sink: Arc<dyn Sink>) -> anyhow::Result<Arc<Self>> {
        let watchdog = p
            .watchdog(DRIVE)
            .ok_or_else(|| anyhow::anyhow!("'{}' has no {DRIVE} lock with a watchdog", p.name()))?;
        let t = Arc::new(Self {
            p: p.clone(),
            sink,
            commands: AtomicU64::new(0),
            estops: AtomicU64::new(0),
            last_seq: Mutex::new(0),
            status: watch::channel(Status {
                tripped: true,
                reason: "no holder".into(),
                ..Default::default()
            })
            .0,
        });
        tokio::spawn(follow(Arc::downgrade(&t), watchdog, p.holders()));
        Ok(t)
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    /// Handles a drive or e-stop message; any other message is handed
    /// back.
    pub fn handle(&self, msg: Message) -> Option<Message> {
        match msg.channel.as_str() {
            DRIVE => {
                let cmd: Command = match msg.json() {
                    Ok(c) => c,
                    Err(e) => {
                        msg.malformed(&e.to_string());
                        return None;
                    }
                };
                match self.sink.apply(&cmd) {
                    Ok(()) => {
                        self.commands.fetch_add(1, Relaxed);
                        *self.last_seq.lock().unwrap() = msg.seq;
                        msg.accept();
                    }
                    Err(e) => msg.reject(&e),
                }
                self.refresh();
                None
            }
            ESTOP => {
                let reason = format!("e-stop from session {}", msg.from_sid);
                self.sink.safe_stop(&reason);
                self.estops.fetch_add(1, Relaxed);
                if let Some(events) = self.p.data(".events") {
                    events.send_json(&serde_json::json!({ "type": "estop", "from_sid": msg.from_sid }));
                }
                msg.accept();
                self.refresh();
                None
            }
            _ => Some(msg),
        }
    }

    fn refresh(&self) {
        self.status.send_modify(|s| {
            s.commands = self.commands.load(Relaxed);
            s.estops = self.estops.load(Relaxed);
            s.last_seq = *self.last_seq.lock().unwrap();
        });
    }
}

async fn follow(
    t: std::sync::Weak<Teleop>,
    mut watchdog: watch::Receiver<tq::WatchdogStatus>,
    mut holders: watch::Receiver<std::collections::HashMap<String, Option<tq::Holder>>>,
) {
    let mut was_tripped = true;
    loop {
        {
            let Some(t) = t.upgrade() else { return };
            let w = watchdog.borrow_and_update().clone();
            let holder = holders
                .borrow_and_update()
                .get(DRIVE)
                .cloned()
                .flatten()
                .map(|h| h.session);
            if w.tripped && !was_tripped {
                t.sink.safe_stop(&w.reason);
            }
            was_tripped = w.tripped;
            t.status.send_modify(|s| {
                s.tripped = w.tripped;
                s.reason = w.reason.clone();
                s.holder = holder;
            });
        }
        tokio::select! {
            r = watchdog.changed() => if r.is_err() { return },
            r = holders.changed() => if r.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertise_adds_a_valid_teleop_surface() {
        let mut ad = Advertisement {
            broadcast: "rover".into(),
            ..Default::default()
        };
        advertise(&mut ad, &Config::default());
        ad.validate().unwrap();
        assert_eq!(ad.channel(DRIVE).unwrap().1.lock.as_deref(), Some(DRIVE));
        assert!(ad.lock(DRIVE).unwrap().1.watchdog.is_some());
        assert!(ad.track(".status").is_some() && ad.track(".events").is_some());
    }
}
