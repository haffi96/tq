//! Wire format of the tq ("tele over QUIC") protocol, 3.x. The protocol
//! version ([`PROTOCOL`]) is separate from each component's version; see
//! [`version`]. The specification is `docs/protocol-3.md`.
//!
//! Every session is a WebTransport session in a room ([`room`]) at
//! `/v1/room/{room}`, and any session may publish (advertise broadcasts),
//! subscribe, or both, as its token's permissions ([`token`]) allow. Each
//! session uses four kinds of channel:
//!
//! - **Control stream**: the first bidirectional stream, opened by the
//!   session. Length-prefixed JSON messages ([`control`]).
//! - **Events stream**: one unidirectional stream each way ([`room`]): the
//!   room's state and events to the session, the edges of its own
//!   broadcasts from it. It and the control stream are sent ahead of media.
//! - **Group streams**: one unidirectional stream per track per group
//!   ([`media`]), in either direction.
//! - **Datagrams** ([`datagram`]): channel messages, lock heartbeats, acks
//!   and echoes, and the frames of datagram tracks.
//!
//! The browser side mirrors this crate in `web/src/protocol.ts`; keep the
//! two in step.

pub mod clock;
pub mod control;
pub mod datagram;
pub mod media;
pub mod path;
pub mod room;
pub mod token;
pub mod version;

pub use clock::*;
pub use control::*;
pub use datagram::*;
pub use media::*;
pub use path::*;
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
    /// Another session took over this session's broadcast.
    pub const REPLACED: u32 = 3;
    /// Session closed because of a protocol violation.
    pub const PROTOCOL_ERROR: u32 = 4;
    /// Session closed by shutdown.
    pub const SHUTDOWN: u32 = 5;
    /// The peers' protocol majors differ; the reason says which versions.
    pub const UNSUPPORTED_VERSION: u32 = 6;
    /// The token does not allow this room.
    pub const FORBIDDEN: u32 = 7;
}
