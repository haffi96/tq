//! tq: the client SDK.
//!
//! A [`Session`] is one connection to a relay room. Through it a program
//! can, as its token allows:
//!
//! - **publish** a [`Publication`]: a broadcast of video and data tracks,
//!   with channels other sessions send into and locks that make some of
//!   them exclusive ([`Session::publish`]);
//! - **subscribe** to any broadcast's tracks ([`Session::subscribe`]);
//! - **send** on channels and hold locks ([`Session::channel`],
//!   [`Session::claim`]);
//! - follow the room's state and events, and put remote timestamps on its
//!   own clock ([`Session::owner_offset`]).
//!
//! A [`Publication`] outlives sessions: sources keep pushing into it while
//! the program reconnects, and each new session publishes it again.
//!
//! ```no_run
//! # async fn demo(opts: tq::Options) -> anyhow::Result<()> {
//! use tq::wire::{Advertisement, DataInfo, Delivery, TrackInfo};
//!
//! let (rover, mut inbox) = tq::Publication::new(Advertisement {
//!     broadcast: "rover-7".into(),
//!     tracks: vec![TrackInfo::data("pose", DataInfo {
//!         encoding: "json".into(),
//!         delivery: Delivery::Latest,
//!         rate_hz: Some(50.0),
//!         window_ms: None,
//!     })],
//!     ..Default::default()
//! })?;
//! let session = tq::Session::connect(opts).await?;
//! session.publish(&rover).await?;
//! rover.data("pose").unwrap().send_json(&serde_json::json!({ "x": 1.0 }));
//! while let Some(msg) = inbox.recv().await {
//!     msg.accept();
//! }
//! # Ok(()) }
//! ```

mod publication;
mod session;
mod watchdog;

pub use publication::{DataWriter, Holder, Inbox, Message, PublishOptions, Publication};
pub use session::{Channel, Frame, Options, Reply, Session, TrackReader};
pub use watchdog::WatchdogStatus;

pub use tq_transport as transport;
pub use tq_transport::quic::{QuicOptions, ServerTrust};
pub use tq_wire as wire;

/// `GET` over plain HTTP/1.0, for a dev relay's `/api/cert` and
/// `/api/info`.
pub async fn http_get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let u = url::Url::parse(url)?;
    anyhow::ensure!(u.scheme() == "http", "{url}: only http:// is supported here");
    let host = u.host_str().context("url has no host")?;
    let port = u.port().unwrap_or(80);
    let mut s = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect((host, port)))
        .await
        .context("HTTP connect timed out")??;
    let path = match u.query() {
        Some(q) => format!("{}?{q}", u.path()),
        None => u.path().to_string(),
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await?;
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut body)).await??;
    let text = String::from_utf8_lossy(&body);
    let json = text.split("\r\n\r\n").nth(1).context("bad HTTP response")?;
    serde_json::from_str(json).with_context(|| format!("{url}: response is not JSON"))
}

/// The relay certificate hash a dev relay serves at `url` (`/api/cert`).
pub async fn fetch_cert_hash(url: &str) -> anyhow::Result<[u8; 32]> {
    use anyhow::Context;
    let v = http_get_json(url).await?;
    tq_transport::cert::parse_hash(v["sha256"].as_str().context("no sha256 field")?)
}
