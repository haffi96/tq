//! A lock's watchdog, run by the broadcast's owner.
//!
//! The lock is **tripped** (safe) unless its holder is attached and alive.
//! It trips when:
//! - there is no holder (initially, on release, or a new holder that has
//!   not sent anything yet);
//! - the holder's heartbeats stop for `heartbeat_loss_ms`;
//! - once messages have started, they stop for `message_stale_ms`;
//! - the relay connection drops.
//!
//! It stays tripped until the next accepted message.

use std::time::{Duration, Instant};

use tq_wire::Watchdog;

/// What apps see of a watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogStatus {
    pub tripped: bool,
    /// Why it last tripped; empty while armed.
    pub reason: String,
    /// Messages accepted from holders, ever.
    pub accepted: u64,
}

impl Default for WatchdogStatus {
    fn default() -> Self {
        Self {
            tripped: true,
            reason: "no holder".into(),
            accepted: 0,
        }
    }
}

/// An edge: the watchdog tripped (with why) or re-armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    Tripped(String),
    Cleared,
}

pub struct State {
    cfg: Watchdog,
    holder: Option<u32>,
    last_heartbeat: Option<Instant>,
    last_message: Option<Instant>,
    started: bool,
    pub status: WatchdogStatus,
}

impl State {
    pub fn new(cfg: Watchdog) -> Self {
        Self {
            cfg,
            holder: None,
            last_heartbeat: None,
            last_message: None,
            started: false,
            status: WatchdogStatus::default(),
        }
    }

    fn trip(&mut self, reason: &str) -> Option<Edge> {
        // The reason always reflects the latest cause; the edge is only
        // reported on the transition.
        self.status.reason = reason.to_string();
        if self.status.tripped {
            return None;
        }
        self.status.tripped = true;
        Some(Edge::Tripped(reason.to_string()))
    }

    #[cfg(test)]
    pub fn holder(&self) -> Option<u32> {
        self.holder
    }

    /// The relay says who holds the lock now.
    pub fn set_holder(&mut self, sid: Option<u32>) -> Option<Edge> {
        if self.holder == sid {
            return if sid.is_none() { self.trip("no holder") } else { None };
        }
        self.holder = sid;
        self.last_heartbeat = None;
        self.last_message = None;
        self.started = false;
        self.trip(if sid.is_some() { "new holder" } else { "no holder" })
    }

    /// The owner's app trips it (an e-stop, say).
    pub fn trip_by_app(&mut self, reason: &str) -> Option<Edge> {
        self.trip(reason)
    }

    pub fn connection_lost(&mut self) -> Option<Edge> {
        self.holder = None;
        self.trip("relay connection lost")
    }

    pub fn heartbeat(&mut self, from: u32) {
        if self.holder == Some(from) {
            self.last_heartbeat = Some(Instant::now());
        }
    }

    /// A message from the holder arrived; a message proves it is alive as
    /// much as a heartbeat does.
    pub fn message(&mut self, from: u32) {
        if self.holder == Some(from) {
            let now = Instant::now();
            self.last_message = Some(now);
            self.last_heartbeat = Some(now);
            self.started = true;
        }
    }

    /// The owner applied a message from the holder: re-arms.
    pub fn accepted(&mut self, from: u32) -> Option<Edge> {
        if self.holder != Some(from) {
            return None;
        }
        self.status.accepted += 1;
        if self.status.tripped {
            self.status.tripped = false;
            self.status.reason.clear();
            return Some(Edge::Cleared);
        }
        None
    }

    /// Runs the timers; call every few tens of milliseconds.
    pub fn tick(&mut self) -> Option<Edge> {
        if self.status.tripped {
            return None;
        }
        let now = Instant::now();
        let hb_loss = Duration::from_millis(self.cfg.heartbeat_loss_ms);
        let stale = Duration::from_millis(self.cfg.message_stale_ms);
        if self.holder.is_none() {
            self.trip("no holder")
        } else if self.last_heartbeat.is_none_or(|t| now - t > hb_loss) {
            self.trip("holder heartbeat lost")
        } else if self.started && self.last_message.is_some_and(|t| now - t > stale) {
            self.trip("messages stale")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wd(hb: u64, stale: u64) -> State {
        State::new(Watchdog {
            heartbeat_loss_ms: hb,
            message_stale_ms: stale,
        })
    }

    #[test]
    fn arms_only_on_accepted_messages_from_the_holder() {
        let mut w = wd(1000, 1000);
        assert!(w.status.tripped);
        assert_eq!(w.accepted(1), None);
        assert_eq!(w.set_holder(Some(1)), None, "already tripped: no edge");
        assert_eq!(w.status.reason, "new holder");
        w.message(2);
        assert_eq!(w.accepted(2), None, "not the holder");
        w.message(1);
        assert_eq!(w.accepted(1), Some(Edge::Cleared));
        assert_eq!(w.accepted(1), None);
        assert_eq!(w.status.accepted, 2);
        assert_eq!(w.set_holder(None), Some(Edge::Tripped("no holder".into())));
    }

    #[test]
    fn trips_on_stale_messages_heartbeat_loss_and_connection_loss() {
        let mut w = wd(1000, 30);
        w.set_holder(Some(1));
        w.message(1);
        w.accepted(1);
        assert_eq!(w.tick(), None);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(w.tick(), Some(Edge::Tripped("messages stale".into())));
        assert_eq!(w.tick(), None);

        let mut w = wd(30, 1000);
        w.set_holder(Some(1));
        w.message(1);
        w.accepted(1);
        std::thread::sleep(Duration::from_millis(40));
        // Messages are older than 30 ms too, but heartbeat loss comes first.
        assert_eq!(w.tick(), Some(Edge::Tripped("holder heartbeat lost".into())));

        let mut w = wd(1000, 1000);
        w.set_holder(Some(1));
        w.message(1);
        w.accepted(1);
        assert_eq!(w.connection_lost(), Some(Edge::Tripped("relay connection lost".into())));
        assert_eq!(w.holder(), None);
    }
}
