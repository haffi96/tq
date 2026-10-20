//! Wire format of the tq ("teleop over QUIC") protocol. The protocol
//! version ([`PROTOCOL`]) is separate from each component's version; see
//! [`version`].
//!
//! Every hop (publisher -> relay -> browser) is a WebTransport session in a
//! room ([`room`]) and uses the same four channels:
//!
//! - **Control stream**: the first bidirectional stream, opened by the
//!   client. Length-prefixed JSON messages ([`control`]) about the session
//!   itself.
//! - **Events stream**: one unidirectional stream per session ([`room`]):
//!   the room's state and its sequenced events. It and the control stream
//!   are sent ahead of video.
//! - **Group streams**: one unidirectional stream per camera per GOP
//!   ([`media`]). A group starts with a keyframe; frames follow in decode
//!   order. Newer groups get a higher send priority, and a sender may reset
//!   an old group's stream to drop it.
//! - **Datagrams**: teleop commands, acks and heartbeats ([`datagram`]),
//!   unreliable and never queued behind video.
//!
//! The browser side mirrors this module in `web/src/protocol.ts`; keep the
//! two in step.

pub mod control;
pub mod datagram;
pub mod media;
pub mod room;
pub mod token;
pub mod version;

pub use control::*;
pub use datagram::*;
pub use media::*;
pub use room::*;
pub use token::*;
pub use version::*;

/// Wall-clock time in Unix microseconds.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Stream / session error codes (WebTransport application error codes).
pub mod code {
    /// A newer group superseded this one.
    pub const GROUP_SUPERSEDED: u32 = 1;
    /// The receiver fell too far behind; the rest of the group was dropped.
    pub const GROUP_LATE: u32 = 2;
    /// Session closed by the relay because another publisher took over.
    pub const REPLACED: u32 = 3;
    /// Session closed because of a protocol violation.
    pub const PROTOCOL_ERROR: u32 = 4;
    /// Session closed by shutdown.
    pub const SHUTDOWN: u32 = 5;
    /// The peers' protocol majors differ; the reason says which versions.
    pub const UNSUPPORTED_VERSION: u32 = 6;
}
