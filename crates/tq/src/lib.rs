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
//!   [`Session::hold`]);
//! - follow the room's state and events, and put remote timestamps on its
//!   own clock ([`Frame::capture_local_us`], [`Frame::hops`]).
//!
//! A [`Client`] is a session that comes back: it reconnects with backoff,
//! resubscribes, and publishes its publications again on each new session.
//! [`Options::discover`] finds a relay from its web address. A
//! [`Publication`] outlives sessions: sources keep pushing into it while
//! the program reconnects.
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

mod client;
pub mod discover;
mod publication;
mod session;
mod watchdog;

pub use client::{Client, Conn, Reader};
pub use discover::{Incompatible, Token};
pub use publication::{DataWriter, Holder, Inbox, Message, PublishOptions, Publication};
pub use session::{Channel, Clocks, Frame, Hold, Hops, Options, Refused, Reply, Session, TrackReader};
pub use watchdog::WatchdogStatus;

pub use tq_transport as transport;
pub use tq_transport::quic::{QuicOptions, ServerTrust};
pub use tq_wire as wire;

/// The relay certificate hash a dev relay serves at `url` (`/api/cert`).
pub async fn fetch_cert_hash(url: &str) -> anyhow::Result<[u8; 32]> {
    use anyhow::Context;
    let v: serde_json::Value = discover::get_json(&url.parse()?).await?;
    tq_transport::cert::parse_hash(v["sha256"].as_str().context("no sha256 field")?)
}
