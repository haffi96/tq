//! Rooms and their events.
//!
//! A room is what a robot's publisher, its operators and its viewers
//! meet in on the relay, named by an id (`/v1/publish/{room}`,
//! `/v1/session/{room}`). Every change a participant should know about
//! (someone joins or leaves, the publisher comes or goes, a track is
//! published, goes offline or comes back, the claim moves, the dead-man
//! trips) is an [`Event`] with the room's next sequence number.
//!
//! Events travel on an **events stream**, a unidirectional stream of its
//! own so that they never wait behind video or the control stream:
//!
//! ```text
//! events stream := type:u8 (=0x02) version:u8 (=1) message*
//! message       := len:u32 (big endian) JSON (a RoomMessage)
//! ```
//!
//! Relay -> client: the relay opens one per session. It starts with a
//! [`RoomMessage::State`], the room as of sequence number `seq`, followed
//! by every event, `seq + 1`, `seq + 2`, … in order. More states follow
//! whenever the room changes (at most every 100 ms; they also carry live
//! numbers such as fps and round trips, which are not events), each only
//! once every event up to its `seq` has been sent. So a client may treat
//! the newest state as the truth and the events as the edges between
//! states. The one exception: a client that falls behind gets a fresh
//! state, with a `seq` ahead of the last event, instead of the events it
//! missed.
//!
//! Publisher -> relay: the publisher opens one per connection and sends
//! the robot-side edges ([`Event::TrackOnline`], [`Event::TrackOffline`],
//! [`Event::DeadmanTripped`], [`Event::DeadmanCleared`]) as
//! [`RoomMessage::Event`]s, numbered per connection and timestamped on the
//! publisher's clock. It starts with the current value of each, so the
//! relay drops the ones that change nothing. The relay renumbers what it
//! accepts into the room's sequence.

use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{BuildInfo, CameraInfo, PublisherStatus, Role, WireError};

pub const STREAM_TYPE_EVENTS: u8 = 0x02;
pub const EVENTS_VERSION: u8 = 1;
/// Largest events stream message accepted.
pub const MAX_ROOM_MESSAGE_LEN: usize = 1 << 20;

/// An operator or viewer session in a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    /// Session id, as in `Welcome` and the operator claim.
    pub session: String,
    pub role: Role,
    /// Who the token was issued to.
    pub sub: String,
    pub build: BuildInfo,
    /// When the session joined, relay clock, Unix microseconds.
    pub joined_us: u64,
}

/// A published camera. Tracks outlive short publisher outages: they go
/// offline when the publisher drops and are unpublished only if it does
/// not return within the relay's grace period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub camera: CameraInfo,
    /// The publisher is connected and the camera's source is producing.
    pub online: bool,
}

/// What every participant knows about a room.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoomState {
    pub room_id: String,
    /// The connected publisher, with its latest numbers.
    pub publisher: Option<PublisherStatus>,
    /// In announce order: a track's index is the `camera` on group streams.
    pub tracks: Vec<TrackInfo>,
    /// Operators and viewers, in join order.
    pub participants: Vec<ParticipantInfo>,
    /// Session id holding the operator claim.
    pub operator: Option<String>,
}

impl RoomState {
    pub fn count(&self, role: Role) -> usize {
        self.participants.iter().filter(|p| p.role == role).count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherGone {
    /// The connection closed or timed out.
    Disconnected,
    /// A new publisher session for the room took over.
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorChange {
    /// Taken while free.
    Claimed,
    /// Taken from another operator.
    Forced,
    /// Given up by the holder.
    Released,
    /// The holder's session ended.
    Left,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ParticipantJoined {
        participant: ParticipantInfo,
    },
    ParticipantLeft {
        participant: ParticipantInfo,
    },
    PublisherConnected {
        publisher_id: String,
        build: Option<BuildInfo>,
    },
    PublisherDisconnected {
        publisher_id: String,
        reason: PublisherGone,
    },
    /// A camera appeared in an announce. Starts offline.
    TrackPublished {
        track: TrackInfo,
    },
    /// A camera's description changed in a new announce (codec, size…).
    TrackUpdated {
        track: TrackInfo,
    },
    TrackUnpublished {
        camera: String,
        reason: String,
    },
    TrackOnline {
        camera: String,
    },
    TrackOffline {
        camera: String,
        reason: String,
    },
    OperatorChanged {
        from: Option<String>,
        to: Option<String>,
        reason: OperatorChange,
    },
    /// The robot safe-stopped.
    DeadmanTripped {
        reason: String,
    },
    /// An accepted command re-armed the robot.
    DeadmanCleared,
    /// An event type from a newer minor of the protocol. Never sent.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// Events the publisher may send; the relay originates the rest.
    pub fn from_robot(&self) -> bool {
        matches!(
            self,
            Event::TrackOnline { .. }
                | Event::TrackOffline { .. }
                | Event::DeadmanTripped { .. }
                | Event::DeadmanCleared
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomEvent {
    pub seq: u64,
    /// When it happened, Unix microseconds (relay clock from the relay;
    /// publisher clock from the publisher).
    pub ts_us: u64,
    pub event: Event,
}

/// Low rate and encoded right away, so the large `State` is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoomMessage {
    /// The room as of event `seq`.
    State {
        seq: u64,
        state: RoomState,
    },
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
            event: Event::OperatorChanged {
                from: None,
                to: Some("abc".into()),
                reason: OperatorChange::Claimed,
            },
        }));
        assert!(json.starts_with(r#"{"type":"event","seq":7,"ts_us":1,"event":{"type":"operator_changed""#));
        assert!(json.contains(r#""reason":"claimed""#));
        let json = roundtrip(&RoomMessage::State {
            seq: 3,
            state: RoomState::default(),
        });
        assert!(json.starts_with(r#"{"type":"state","seq":3,"#));
        roundtrip(&RoomMessage::Event(RoomEvent {
            seq: 8,
            ts_us: 2,
            event: Event::DeadmanCleared,
        }));
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
    fn robot_events() {
        assert!(Event::DeadmanCleared.from_robot());
        assert!(!Event::TrackUnpublished {
            camera: "a".into(),
            reason: String::new()
        }
        .from_robot());
    }
}
