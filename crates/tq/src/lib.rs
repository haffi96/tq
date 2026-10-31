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
//! # async fn demo(encoder: impl Iterator<Item = Vec<u8>> + Send + 'static) -> anyhow::Result<()> {
//! let (rover, mut inbox) = tq::Publication::builder("rover-7").video("front").channel("lights").build()?;
//! let token = tq::Token::publisher("shared-secret", "rover-7");
//! let client = tq::Client::discover("http://localhost:8080", "robot-01", token)?;
//! client.publish(&rover, Default::default());
//! let mut front = rover.video("front").unwrap();
//! std::thread::spawn(move || encoder.for_each(|au| drop(front.write_frame(&au))));
//! while let Some(msg) = inbox.recv().await {
//!     msg.accept();
//! }
//! # Ok(()) }
//! ```
//!
//! `examples/camera.rs` runs this against a dev relay. Data tracks are
//! written the same way ([`Publication::data`]); [`Publication::builder`]
//! takes any track, channel or lock, and [`Session`] gives full control of
//! one connection.

mod client;
pub mod discover;
mod publication;
mod session;
mod video;
mod watchdog;

pub use client::{Client, Conn, Reader};
pub use discover::{Incompatible, Token};
pub use publication::{Builder, DataWriter, Holder, Inbox, Message, PublishOptions, Publication};
pub use session::{Channel, Clocks, Frame, Hold, Hops, Options, Refused, Reply, Session, TrackReader};
pub use video::{VideoOptions, VideoWriter};
pub use watchdog::WatchdogStatus;

pub use tq_media as media;
pub use tq_transport as transport;
pub use tq_transport::quic::{QuicOptions, ServerTrust};
pub use tq_wire as wire;

/// The relay certificate hash a dev relay serves at `url` (`/api/cert`).
pub async fn fetch_cert_hash(url: &str) -> anyhow::Result<[u8; 32]> {
    use anyhow::Context;
    let v: serde_json::Value = discover::get_json(&url.parse()?).await?;
    tq_transport::cert::parse_hash(v["sha256"].as_str().context("no sha256 field")?)
}
