//! Teleop command handling and the AV-side dead-man switch.
//!
//! The vehicle is safe-stopped (tripped) unless an operator is attached and
//! alive. It trips when:
//! - there is no operator (initially, on release, or a new operator who has
//!   not sent anything yet);
//! - the operator's heartbeats stop for `heartbeat_loss_ms`;
//! - once commands have started, they stop for `command_stale_ms`;
//! - the relay connection drops.
//!
//! It stays tripped until the next accepted command. Commands are
//! latest-wins: a sequence number at or below the last applied one is
//! acknowledged as superseded and not applied.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Deserialize;
use tq_proto::{AckStatus, Datagram, TeleopStatus};

use crate::config::TeleopConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct TeleopCommand {
    pub command: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Where accepted commands go. The vehicle interface is not part of this
/// repo; [`LoggingSink`] stands in for it.
pub trait TeleopSink: Send + Sync {
    fn apply(&self, cmd: &TeleopCommand) -> Result<(), String>;
    fn safe_stop(&self, reason: &str);
}

pub struct LoggingSink;

impl TeleopSink for LoggingSink {
    fn apply(&self, cmd: &TeleopCommand) -> Result<(), String> {
        tracing::debug!(command = %cmd.command, args = %cmd.args, "teleop apply");
        Ok(())
    }

    fn safe_stop(&self, reason: &str) {
        tracing::warn!(reason, "teleop SAFE STOP");
    }
}

struct State {
    operator: Option<String>,
    last_seq: Option<u32>,
    last_heartbeat: Option<Instant>,
    last_command: Option<Instant>,
    commands_started: bool,
    tripped: bool,
    reason: String,
    commands: u64,
}

pub struct Teleop {
    cfg: TeleopConfig,
    sink: Arc<dyn TeleopSink>,
    state: Mutex<State>,
}

impl Teleop {
    pub fn new(cfg: TeleopConfig, sink: Arc<dyn TeleopSink>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            sink,
            state: Mutex::new(State {
                operator: None,
                last_seq: None,
                last_heartbeat: None,
                last_command: None,
                commands_started: false,
                tripped: true,
                reason: "no operator".into(),
                commands: 0,
            }),
        })
    }

    fn trip(&self, s: &mut State, reason: &str) {
        // The reason always reflects the latest cause; the sink is only
        // told on the transition.
        s.reason = reason.to_string();
        if !s.tripped {
            s.tripped = true;
            self.sink.safe_stop(reason);
        }
    }

    /// The relay says which operator session now holds the claim.
    pub fn set_operator(&self, session: Option<String>) {
        let mut s = self.state.lock().unwrap();
        if s.operator == session {
            if session.is_none() {
                self.trip(&mut s, "no operator");
            }
            return;
        }
        tracing::info!(operator = ?session, "operator changed");
        s.operator = session;
        s.last_seq = None;
        s.last_heartbeat = None;
        s.last_command = None;
        s.commands_started = false;
        let reason = if s.operator.is_some() {
            "new operator"
        } else {
            "no operator"
        };
        self.trip(&mut s, reason);
    }

    pub fn connection_lost(&self) {
        let mut s = self.state.lock().unwrap();
        s.operator = None;
        self.trip(&mut s, "relay connection lost");
    }

    /// Handles a datagram from the operator; returns the reply to send.
    pub fn handle(&self, dg: Datagram) -> Option<Datagram> {
        let mut s = self.state.lock().unwrap();
        match dg {
            Datagram::Heartbeat { seq, sent_us } => {
                s.operator.as_ref()?;
                s.last_heartbeat = Some(Instant::now());
                Some(Datagram::Echo {
                    seq,
                    echo_us: sent_us,
                    tripped: s.tripped,
                })
            }
            Datagram::Command { seq, sent_us, body } => {
                if s.operator.is_none() {
                    return Some(ack(seq, sent_us, AckStatus::Rejected, "no operator"));
                }
                let cmd: TeleopCommand = match serde_json::from_slice(&body) {
                    Ok(c) => c,
                    Err(e) => return Some(ack(seq, sent_us, AckStatus::Malformed, &e.to_string())),
                };
                if s.last_seq.is_some_and(|last| seq <= last) {
                    return Some(ack(seq, sent_us, AckStatus::Superseded, "older than last applied"));
                }
                s.last_seq = Some(seq);
                let now = Instant::now();
                s.last_command = Some(now);
                // A command proves the operator is alive as much as a
                // heartbeat does.
                s.last_heartbeat = Some(now);
                s.commands_started = true;
                match self.sink.apply(&cmd) {
                    Ok(()) => {
                        s.commands += 1;
                        if s.tripped {
                            tracing::info!(seq, "teleop re-armed by command");
                            s.tripped = false;
                            s.reason.clear();
                        }
                        Some(ack(seq, sent_us, AckStatus::Ok, ""))
                    }
                    Err(e) => Some(ack(seq, sent_us, AckStatus::Rejected, &e)),
                }
            }
            _ => None,
        }
    }

    /// Runs the dead-man timers; call every few tens of milliseconds.
    pub fn tick(&self) {
        let mut s = self.state.lock().unwrap();
        if s.tripped {
            return;
        }
        let now = Instant::now();
        let hb_loss = Duration::from_millis(self.cfg.heartbeat_loss_ms);
        let stale = Duration::from_millis(self.cfg.command_stale_ms);
        if s.operator.is_none() {
            self.trip(&mut s, "no operator");
        } else if s.last_heartbeat.is_none_or(|t| now - t > hb_loss) {
            self.trip(&mut s, "operator heartbeat lost");
        } else if s.commands_started && s.last_command.is_some_and(|t| now - t > stale) {
            self.trip(&mut s, "teleop commands stale");
        }
    }

    pub fn status(&self) -> TeleopStatus {
        let s = self.state.lock().unwrap();
        TeleopStatus {
            deadman_tripped: s.tripped,
            reason: s.reason.clone(),
            last_seq: s.last_seq.unwrap_or(0),
            commands: s.commands,
            operator: s.operator.clone(),
        }
    }
}

fn ack(seq: u32, echo_us: u64, status: AckStatus, detail: &str) -> Datagram {
    Datagram::Ack {
        seq,
        echo_us,
        status,
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Count {
        stops: AtomicUsize,
        applied: AtomicUsize,
    }
    impl TeleopSink for Count {
        fn apply(&self, _: &TeleopCommand) -> Result<(), String> {
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn safe_stop(&self, _: &str) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn cmd(seq: u32) -> Datagram {
        Datagram::Command {
            seq,
            sent_us: 1,
            body: Bytes::from_static(br#"{"command":"drive","args":{"v":1}}"#),
        }
    }

    #[test]
    fn latest_wins_and_rearm() {
        let sink = Arc::new(Count::default());
        let t = Teleop::new(
            TeleopConfig {
                heartbeat_loss_ms: 50,
                command_stale_ms: 50,
            },
            sink.clone(),
        );
        assert!(matches!(
            t.handle(cmd(1)),
            Some(Datagram::Ack {
                status: AckStatus::Rejected,
                ..
            })
        ));
        t.set_operator(Some("op".into()));
        assert!(t.status().deadman_tripped);
        assert!(matches!(
            t.handle(cmd(2)),
            Some(Datagram::Ack {
                status: AckStatus::Ok,
                ..
            })
        ));
        assert!(!t.status().deadman_tripped);
        assert!(matches!(
            t.handle(cmd(2)),
            Some(Datagram::Ack {
                status: AckStatus::Superseded,
                ..
            })
        ));
        assert!(matches!(
            t.handle(cmd(1)),
            Some(Datagram::Ack {
                status: AckStatus::Superseded,
                ..
            })
        ));
        assert_eq!(sink.applied.load(Ordering::SeqCst), 1);
        let bad = Datagram::Command {
            seq: 9,
            sent_us: 0,
            body: Bytes::from_static(b"nope"),
        };
        assert!(matches!(
            t.handle(bad),
            Some(Datagram::Ack {
                status: AckStatus::Malformed,
                ..
            })
        ));
    }

    #[test]
    fn trips_on_stale_commands_and_connection_loss() {
        let sink = Arc::new(Count::default());
        let t = Teleop::new(
            TeleopConfig {
                heartbeat_loss_ms: 1000,
                command_stale_ms: 30,
            },
            sink.clone(),
        );
        t.set_operator(Some("op".into()));
        t.handle(cmd(1));
        t.tick();
        assert!(!t.status().deadman_tripped);
        std::thread::sleep(Duration::from_millis(40));
        t.tick();
        let st = t.status();
        assert!(st.deadman_tripped);
        assert_eq!(st.reason, "teleop commands stale");
        assert_eq!(sink.stops.load(Ordering::SeqCst), 1);
        t.handle(cmd(2));
        t.connection_lost();
        assert_eq!(t.status().reason, "relay connection lost");
        assert_eq!(sink.stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn heartbeat_loss() {
        let t = Teleop::new(
            TeleopConfig {
                heartbeat_loss_ms: 30,
                command_stale_ms: 1000,
            },
            Arc::new(LoggingSink),
        );
        t.set_operator(Some("op".into()));
        t.handle(cmd(1));
        assert!(matches!(
            t.handle(Datagram::Heartbeat { seq: 1, sent_us: 5 }),
            Some(Datagram::Echo {
                echo_us: 5,
                tripped: false,
                ..
            })
        ));
        std::thread::sleep(Duration::from_millis(40));
        t.tick();
        // Commands are older than 30 ms too, but heartbeat loss is checked first.
        assert_eq!(t.status().reason, "operator heartbeat lost");
    }
}
