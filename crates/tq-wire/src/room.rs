//! Rooms and their events.
//!
//! A room is a namespace on the relay (`/v1/room/{room}`) where sessions
//! publish broadcasts and subscribe to their tracks. Every change a session
//! should know about (someone joins or leaves, a broadcast starts or stops,
//! a track is published, goes offline or comes back, a lock moves) is an
//! [`Event`] with the room's next sequence number. What happens *inside* a
//! broadcast (a robot's status, its watchdog, its log) is not a room event:
//! it goes on the broadcast's own data tracks (`.status`, `.events`).
//!
//! Events travel on an **events stream**, a unidirectional stream of its
//! own so that they never wait behind media or the control stream:
//!
//! ```text
//! events stream := type:u8 (=0x02) version:u8 (=2) message*
//! message       := len:u32 (big endian) JSON (a RoomMessage)
//! ```
//!
//! Relay -> session: the relay opens one per session. It starts with a
//! [`RoomMessage::State`], the room as of sequence number `seq`, followed
//! by every event, `seq + 1`, `seq + 2`, … in order. More states follow
//! whenever the room changes (at most every 100 ms; they also carry live
//! numbers such as fps and round trips, which are not events), each only
//! once every event up to its `seq` has been sent. So a session may treat
//! the newest state as the truth and the events as the edges between
//! states. The one exception: a session that falls behind gets a fresh
//! state, with a `seq` ahead of the last event, instead of the events it
//! missed.
//!
//! Session -> relay: a session that publishes opens one and sends its
//! tracks' edges ([`Event::TrackOnline`], [`Event::TrackOffline`]) as
//! [`RoomMessage::Event`]s, numbered per connection and timestamped on its
//! own clock. It starts with the current value of each, so the relay drops
//! the ones that change nothing. The relay renumbers what it accepts into
//! the room's sequence.

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{BuildInfo, ChannelInfo, LockInfo, Path, Perms, QuicStats, TrackInfo, WireError};

pub const STREAM_TYPE_EVENTS: u8 = 0x02;
pub const EVENTS_VERSION: u8 = 2;
/// Largest events stream message accepted.
pub const MAX_ROOM_MESSAGE_LEN: usize = 1 << 20;

/// A session in a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    /// Session id, as in `welcome` and lock holders.
    pub session: String,
    pub sid: u32,
    /// Who the token was issued to.
    pub sub: String,
    pub perms: Perms,
    pub build: BuildInfo,
    /// When the session joined, relay clock, Unix microseconds.
    pub joined_us: u64,
}

/// Numbers the relay measures on a track as it arrives, plus what only the
/// owner knows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackStats {
    pub frames: u64,
    pub groups: u64,
    pub fps: f64,
    pub kbps: f64,
    /// Dropped on the owner -> relay hop, as the owner reports it.
    pub dropped: u64,
    /// Mean capture -> owner latency over the last second, ms (SEI only).
    pub capture_to_publisher_ms: Option<f64>,
    /// WebCodecs string of the latest video group.
    pub codec_string: Option<String>,
    pub subscribers: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackState {
    /// Position in the advertisement: the `track` on group streams.
    pub index: u16,
    pub info: TrackInfo,
    /// The owner is connected and the track's source is producing.
    pub online: bool,
    pub stats: TrackStats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelState {
    pub index: u16,
    pub info: ChannelInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockState {
    pub index: u16,
    pub info: LockInfo,
    /// Session holding it.
    pub holder: Option<String>,
}

/// A broadcast in the room. It outlives short owner outages: its tracks go
/// offline when the owner drops and it is removed only if no owner returns
/// within the relay's grace period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BroadcastState {
    /// The room's index: `broadcast` on the subscriber hop.
    pub index: u16,
    pub name: String,
    /// The owning session, while connected.
    pub owner: Option<String>,
    pub owner_sid: Option<u32>,
    pub build: Option<BuildInfo>,
    /// Relay <-> owner round trip (control stream ping), ms.
    pub rtt_ms: Option<f64>,
    /// Owner clock minus relay clock, microseconds.
    pub clock_offset_us: Option<i64>,
    /// Bound on the offset's error: half the best round trip it came from.
    pub clock_error_us: Option<i64>,
    /// QUIC path stats of the owner -> relay connection.
    pub quic: Option<QuicStats>,
    pub tracks: Vec<TrackState>,
    pub channels: Vec<ChannelState>,
    pub locks: Vec<LockState>,
}

impl BroadcastState {
    pub fn track(&self, id: &str) -> Option<&TrackState> {
        self.tracks.iter().find(|t| t.info.id == id)
    }

    pub fn lock(&self, id: &str) -> Option<&LockState> {
        self.locks.iter().find(|l| l.info.id == id)
    }

    pub fn channel(&self, id: &str) -> Option<&ChannelState> {
        self.channels.iter().find(|c| c.info.id == id)
    }
}

/// What every session knows about a room.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomState {
    pub room_id: String,
    pub broadcasts: Vec<BroadcastState>,
    /// In join order.
    pub participants: Vec<ParticipantInfo>,
}

impl RoomState {
    pub fn broadcast(&self, name: &str) -> Option<&BroadcastState> {
        self.broadcasts.iter().find(|b| b.name == name)
    }

    pub fn broadcast_at(&self, index: u16) -> Option<&BroadcastState> {
        self.broadcasts.iter().find(|b| b.index == index)
    }

    pub fn track(&self, path: &Path) -> Option<&TrackState> {
        self.broadcast(&path.broadcast)?.track(&path.name)
    }

    pub fn lock(&self, path: &Path) -> Option<&LockState> {
        self.broadcast(&path.broadcast)?.lock(&path.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The owner's connection closed or timed out.
    Disconnected,
    /// Another session took the broadcast over.
    Replaced,
    /// The owner unadvertised it.
    Unadvertised,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockChange {
    /// Taken while free.
    Claimed,
    /// Taken from another session.
    Forced,
    /// Given up by the holder.
    Released,
    /// The holder's session ended, or the lock went away.
    Left,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ParticipantJoined {
        participant: ParticipantInfo,
    },
    ParticipantLeft {
        participant: ParticipantInfo,
    },
    /// A session took ownership of a broadcast: new, back after an outage,
    /// or replacing another session.
    BroadcastStarted {
        broadcast: String,
        owner: String,
        build: Option<BuildInfo>,
    },
    /// The owner went away; the broadcast stays, offline, for the grace
    /// period (unless unadvertised, which removes it at once).
    BroadcastStopped {
        broadcast: String,
        reason: StopReason,
    },
    BroadcastRemoved {
        broadcast: String,
    },
    /// A track appeared in an advertisement. Starts offline.
    TrackPublished {
        track: Path,
        info: TrackInfo,
    },
    /// A track's description changed in a new advertisement.
    TrackUpdated {
        track: Path,
        info: TrackInfo,
    },
    TrackUnpublished {
        track: Path,
    },
    TrackOnline {
        track: Path,
    },
    TrackOffline {
        track: Path,
        reason: String,
    },
    LockChanged {
        lock: Path,
        from: Option<String>,
        to: Option<String>,
        reason: LockChange,
    },
    /// An event type from a newer minor of the protocol. Never sent.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// Events a session may send about its own broadcasts; the relay
    /// originates the rest.
    pub fn from_owner(&self) -> bool {
        matches!(self, Event::TrackOnline { .. } | Event::TrackOffline { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomEvent {
    pub seq: u64,
    /// When it happened, Unix microseconds (relay clock from the relay;
    /// the sender's clock from a session).
    pub ts_us: u64,
    pub event: Event,
}

/// Low rate and encoded right away, so the large `State` is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoomMessage {
    /// The room as of event `seq`.
    State { seq: u64, state: RoomState },
    Event(RoomEvent),
    /// A message type from a newer minor of the protocol. Never sent.
    #[serde(other)]
    Unknown,
}

impl RoomMessage {
    /// The length-prefixed JSON encoding.
    pub fn encode(&self) -> Bytes {
        let json = serde_json::to_vec(self).expect("room messages serialize");
        let mut buf = BytesMut::with_capacity(4 + json.len());
        buf.put_u32(json.len() as u32);
        buf.put_slice(&json);
        buf.freeze()
    }

    pub fn check_len(len: u32) -> Result<usize, WireError> {
        let len = len as usize;
        if len == 0 || len > MAX_ROOM_MESSAGE_LEN {
            return Err(WireError::FrameLen(len));
        }
        Ok(len)
    }

    pub fn decode(json: &[u8]) -> Result<Self, WireError> {
        serde_json::from_slice(json).map_err(|e| WireError::Json(e.to_string()))
    }
}

/// The two bytes an events stream starts with.
pub fn events_stream_header() -> [u8; 2] {
    [STREAM_TYPE_EVENTS, EVENTS_VERSION]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: &RoomMessage) -> String {
        let b = m.encode();
        let len = RoomMessage::check_len(u32::from_be_bytes(b[..4].try_into().unwrap())).unwrap();
        assert_eq!(len, b.len() - 4);
        assert_eq!(&RoomMessage::decode(&b[4..]).unwrap(), m);
        String::from_utf8(b[4..].to_vec()).unwrap()
    }

    #[test]
    fn event_wire_shape() {
        let json = roundtrip(&RoomMessage::Event(RoomEvent {
            seq: 7,
            ts_us: 1,
            event: Event::LockChanged {
                lock: "rover/drive".parse().unwrap(),
                from: None,
                to: Some("abc".into()),
                reason: LockChange::Claimed,
            },
        }));
        assert!(json.starts_with(
            r#"{"type":"event","seq":7,"ts_us":1,"event":{"type":"lock_changed","lock":"rover/drive""#
        ));
        assert!(json.contains(r#""reason":"claimed""#));
        let json = roundtrip(&RoomMessage::State {
            seq: 3,
            state: RoomState::default(),
        });
        assert!(json.starts_with(r#"{"type":"state","seq":3,"#));
    }

    #[test]
    fn unknown_types_are_tolerated() {
        assert_eq!(
            RoomMessage::decode(br#"{"type":"from_the_future"}"#).unwrap(),
            RoomMessage::Unknown
        );
        let m =
            RoomMessage::decode(br#"{"type":"event","seq":1,"ts_us":0,"event":{"type":"new_thing","x":1}}"#).unwrap();
        assert!(matches!(
            m,
            RoomMessage::Event(RoomEvent {
                event: Event::Unknown,
                ..
            })
        ));
    }

    #[test]
    fn owner_events() {
        assert!(Event::TrackOnline {
            track: "a/b".parse().unwrap()
        }
        .from_owner());
        assert!(!Event::TrackUnpublished {
            track: "a/b".parse().unwrap()
        }
        .from_owner());
    }
}
