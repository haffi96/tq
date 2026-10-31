//! Sinks: where channel messages (and teleop's drive commands) go.
//!
//! - `log`: logs them. A stand-in until something real is connected.
//! - `tcp`: tq-edge connects to a TCP server (your program) and writes one
//!   JSON object per line. A message is acked `ok` once handed to the
//!   connection, `rejected` while there is none; it is never queued across
//!   a reconnect, so a stale command cannot arrive late. What the program
//!   writes back is ignored.
//!
//! Lines a `tcp` sink writes:
//!
//! ```text
//! {"type":"message","channel":"lights","from_sid":3,"seq":7,"sent_us":…,"payload":{…}}
//! {"type":"command","command":"drive","args":{"throttle":0.5,"steer":0}}   (teleop)
//! {"type":"safe_stop","reason":"messages stale"}                          (teleop)
//! ```
//!
//! A teleop sink writes a `safe_stop` first on every connect, so a motion
//! program that (re)connects starts stopped. That program should stop by
//! itself when the connection drops.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use bytes::Bytes;
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};
use tq::Message;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Lines waiting to be written; beyond that, messages are rejected.
const QUEUE: usize = 256;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum SinkConfig {
    #[default]
    Log,
    Tcp {
        /// `host:port` of the program to write to.
        addr: String,
    },
}

/// A started sink.
pub enum Sink {
    Log(String),
    Tcp(TcpSink),
}

impl Sink {
    /// Starts a sink named `name` (for logs). `hello` is written first on
    /// every connect of a `tcp` sink.
    pub fn start(name: &str, cfg: &SinkConfig, hello: Option<serde_json::Value>) -> Self {
        match cfg {
            SinkConfig::Log => Sink::Log(name.to_string()),
            SinkConfig::Tcp { addr } => Sink::Tcp(TcpSink::start(name, addr, hello)),
        }
    }

    /// Writes one JSON line; `Err` with why it could not.
    pub fn write(&self, line: &serde_json::Value) -> Result<(), String> {
        match self {
            Sink::Log(name) => {
                tracing::info!(sink = %name, %line, "sink");
                Ok(())
            }
            Sink::Tcp(t) => t.write(line),
        }
    }

    /// Hands a channel message on and acks it.
    pub fn deliver(&self, msg: Message) {
        let payload: serde_json::Value = match msg.json() {
            Ok(v) => v,
            Err(e) => return msg.malformed(&format!("not JSON: {e}")),
        };
        let line = serde_json::json!({
            "type": "message",
            "channel": msg.channel,
            "from_sid": msg.from_sid,
            "seq": msg.seq,
            "sent_us": msg.sent_us,
            "payload": payload,
        });
        match self.write(&line) {
            Ok(()) => msg.accept(),
            Err(e) => msg.reject(&e),
        }
    }
}

pub struct TcpSink {
    tx: mpsc::Sender<Bytes>,
    connected: Arc<AtomicBool>,
}

impl TcpSink {
    fn start(name: &str, addr: &str, hello: Option<serde_json::Value>) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let connected = Arc::new(AtomicBool::new(false));
        let hello = hello.map(|v| line_bytes(&v));
        tokio::spawn(write_loop(name.to_string(), addr.to_string(), hello, rx, connected.clone()));
        Self { tx, connected }
    }

    fn write(&self, line: &serde_json::Value) -> Result<(), String> {
        if !self.connected.load(Relaxed) {
            return Err("sink not connected".into());
        }
        self.tx.try_send(line_bytes(line)).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => "sink busy".to_string(),
            mpsc::error::TrySendError::Closed(_) => "sink stopped".to_string(),
        })
    }
}

fn line_bytes(v: &serde_json::Value) -> Bytes {
    let mut b = serde_json::to_vec(v).expect("a JSON value serializes");
    b.push(b'\n');
    b.into()
}

async fn write_loop(
    name: String,
    addr: String,
    hello: Option<Bytes>,
    mut rx: mpsc::Receiver<Bytes>,
    connected: Arc<AtomicBool>,
) {
    let mut logged_down = false;
    loop {
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                logged_down = false;
                let _ = stream.set_nodelay(true);
                let (mut rd, mut wr) = stream.into_split();
                // Nothing from before this connection goes out on it.
                while rx.try_recv().is_ok() {}
                let ok = match &hello {
                    Some(h) => wr.write_all(h).await.is_ok(),
                    None => true,
                };
                if ok {
                    tracing::info!(sink = %name, %addr, "sink connected");
                    connected.store(true, Relaxed);
                    // The program closing its end is noticed at once, not
                    // on the next write (which would be acked and lost).
                    let mut scratch = [0u8; 512];
                    loop {
                        tokio::select! {
                            line = rx.recv() => {
                                let Some(line) = line else { return };
                                if let Err(e) = wr.write_all(&line).await {
                                    tracing::warn!(sink = %name, error = %e, "sink write failed");
                                    break;
                                }
                            }
                            r = rd.read(&mut scratch) => if !matches!(r, Ok(n) if n > 0) {
                                tracing::warn!(sink = %name, "sink closed");
                                break;
                            },
                        }
                    }
                    connected.store(false, Relaxed);
                }
            }
            Err(e) => {
                if !logged_down {
                    tracing::warn!(sink = %name, %addr, error = %e, "sink unavailable, retrying every second");
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Teleop's motion interface on a sink.
pub struct TeleopSink(pub Sink);

impl tq_teleop::Sink for TeleopSink {
    fn apply(&self, cmd: &tq_teleop::Command) -> Result<(), String> {
        self.0.write(&serde_json::json!({ "type": "command", "command": cmd.command, "args": cmd.args }))
    }

    fn safe_stop(&self, reason: &str) {
        tracing::warn!(reason, "teleop SAFE STOP");
        if let Err(e) = self.0.write(&serde_json::json!({ "type": "safe_stop", "reason": reason })) {
            // A motion program that is not connected must already be
            // stopped (see the module docs).
            tracing::warn!(error = %e, "safe stop not delivered to the sink");
        }
    }
}
