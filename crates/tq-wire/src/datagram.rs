//! Datagrams: channel messages into a broadcast, the owner's acks, lock
//! heartbeats and their echoes, and the frames of datagram tracks.
//!
//! ```text
//! 0x20 message      sid:u32 broadcast:u16 channel:u16 seq:u32 sent_us:u64 payload
//! 0x21 ack          sid:u32 broadcast:u16 channel:u16 seq:u32 echo_us:u64 status:u8 detail:utf8
//! 0x22 heartbeat    sid:u32 broadcast:u16 lock:u16    seq:u32 sent_us:u64
//! 0x23 echo         sid:u32 broadcast:u16 lock:u16    seq:u32 echo_us:u64 flags:u8
//! 0x24 track_frame  broadcast:u16 track:u16 seq:u32 flags:u8
//!                   capture_us:u64 publisher_us:u64 relay_us:u64 payload
//! ```
//!
//! `sid` routes channel traffic: a sender writes 0 and the relay writes the
//! sender's sid on the way to the owner; the owner writes the target's sid
//! on acks and echoes, and the relay forwards them to that session only.
//! `broadcast` follows the same rule as on group streams: the owner's own
//! index on the owner hop, the room's index on the subscriber hop.
//!
//! `sent_us` is on the sender's clock and only ever compared with itself
//! (echoed back), so no clock sync is needed for round trips.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::WireError;

pub const DG_MESSAGE: u8 = 0x20;
pub const DG_ACK: u8 = 0x21;
pub const DG_HEARTBEAT: u8 = 0x22;
pub const DG_ECHO: u8 = 0x23;
pub const DG_TRACK_FRAME: u8 = 0x24;

/// Largest datagram a sender may write: fits one QUIC datagram on any path
/// with room to spare.
pub const MAX_DATAGRAM: usize = 1100;
/// Bytes before a message's payload.
pub const MESSAGE_HEADER_LEN: usize = 1 + 4 + 2 + 2 + 4 + 8;
/// Bytes before a track frame's payload.
pub const TRACK_FRAME_HEADER_LEN: usize = 1 + 2 + 2 + 4 + 1 + 8 + 8 + 8;

/// Echo flag: the lock's watchdog has tripped.
pub const ECHO_TRIPPED: u8 = 1 << 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AckStatus {
    Ok = 0,
    /// An equal or newer sequence number was already applied.
    Superseded = 1,
    /// The owner refused the message.
    Rejected = 2,
    /// The payload could not be parsed.
    Malformed = 3,
}

impl AckStatus {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => AckStatus::Ok,
            1 => AckStatus::Superseded,
            2 => AckStatus::Rejected,
            _ => AckStatus::Malformed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datagram {
    Message {
        sid: u32,
        broadcast: u16,
        channel: u16,
        seq: u32,
        sent_us: u64,
        payload: Bytes,
    },
    Ack {
        sid: u32,
        broadcast: u16,
        channel: u16,
        seq: u32,
        echo_us: u64,
        status: AckStatus,
        detail: String,
    },
    Heartbeat {
        sid: u32,
        broadcast: u16,
        lock: u16,
        seq: u32,
        sent_us: u64,
    },
    Echo {
        sid: u32,
        broadcast: u16,
        lock: u16,
        seq: u32,
        echo_us: u64,
        flags: u8,
    },
    TrackFrame {
        broadcast: u16,
        track: u16,
        seq: u32,
        flags: u8,
        capture_us: u64,
        publisher_us: u64,
        relay_us: u64,
        payload: Bytes,
    },
}

impl Datagram {
    pub fn encode(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(48);
        match self {
            Datagram::Message {
                sid,
                broadcast,
                channel,
                seq,
                sent_us,
                payload,
            } => {
                b.put_u8(DG_MESSAGE);
                put_route(&mut b, *sid, *broadcast, *channel, *seq, *sent_us);
                b.put_slice(payload);
            }
            Datagram::Ack {
                sid,
                broadcast,
                channel,
                seq,
                echo_us,
                status,
                detail,
            } => {
                b.put_u8(DG_ACK);
                put_route(&mut b, *sid, *broadcast, *channel, *seq, *echo_us);
                b.put_u8(*status as u8);
                b.put_slice(detail.as_bytes());
            }
            Datagram::Heartbeat {
                sid,
                broadcast,
                lock,
                seq,
                sent_us,
            } => {
                b.put_u8(DG_HEARTBEAT);
                put_route(&mut b, *sid, *broadcast, *lock, *seq, *sent_us);
            }
            Datagram::Echo {
                sid,
                broadcast,
                lock,
                seq,
                echo_us,
                flags,
            } => {
                b.put_u8(DG_ECHO);
                put_route(&mut b, *sid, *broadcast, *lock, *seq, *echo_us);
                b.put_u8(*flags);
            }
            Datagram::TrackFrame {
                broadcast,
                track,
                seq,
                flags,
                capture_us,
                publisher_us,
                relay_us,
                payload,
            } => {
                b.put_u8(DG_TRACK_FRAME);
                b.put_u16(*broadcast);
                b.put_u16(*track);
                b.put_u32(*seq);
                b.put_u8(*flags);
                b.put_u64(*capture_us);
                b.put_u64(*publisher_us);
                b.put_u64(*relay_us);
                b.put_slice(payload);
            }
        }
        b.freeze()
    }

    pub fn decode(mut b: Bytes) -> Result<Self, WireError> {
        let Some(&ty) = b.first() else {
            return Err(WireError::Truncated);
        };
        if ty == DG_TRACK_FRAME {
            if b.len() < TRACK_FRAME_HEADER_LEN {
                return Err(WireError::Truncated);
            }
            b.advance(1);
            return Ok(Datagram::TrackFrame {
                broadcast: b.get_u16(),
                track: b.get_u16(),
                seq: b.get_u32(),
                flags: b.get_u8(),
                capture_us: b.get_u64(),
                publisher_us: b.get_u64(),
                relay_us: b.get_u64(),
                payload: b,
            });
        }
        if !(DG_MESSAGE..=DG_ECHO).contains(&ty) {
            return Err(WireError::DatagramType(ty));
        }
        if b.len() < MESSAGE_HEADER_LEN + matches!(ty, DG_ACK | DG_ECHO) as usize {
            return Err(WireError::Truncated);
        }
        b.advance(1);
        let (sid, broadcast, id, seq, t) = (b.get_u32(), b.get_u16(), b.get_u16(), b.get_u32(), b.get_u64());
        Ok(match ty {
            DG_MESSAGE => Datagram::Message {
                sid,
                broadcast,
                channel: id,
                seq,
                sent_us: t,
                payload: b,
            },
            DG_ACK => {
                let status = AckStatus::from_u8(b.get_u8());
                Datagram::Ack {
                    sid,
                    broadcast,
                    channel: id,
                    seq,
                    echo_us: t,
                    status,
                    detail: String::from_utf8_lossy(&b).into_owned(),
                }
            }
            DG_HEARTBEAT => Datagram::Heartbeat {
                sid,
                broadcast,
                lock: id,
                seq,
                sent_us: t,
            },
            _ => Datagram::Echo {
                sid,
                broadcast,
                lock: id,
                seq,
                echo_us: t,
                flags: b.get_u8(),
            },
        })
    }

    /// Datagram type byte without decoding the rest.
    pub fn peek_type(b: &[u8]) -> Option<u8> {
        b.first().copied()
    }
}

fn put_route(b: &mut BytesMut, sid: u32, broadcast: u16, id: u16, seq: u32, t: u64) {
    b.put_u32(sid);
    b.put_u16(broadcast);
    b.put_u16(id);
    b.put_u32(seq);
    b.put_u64(t);
}

/// `(sid, broadcast, id)` of a channel datagram (types 0x20–0x23), read in
/// place, for routing without decoding.
pub fn peek_route(b: &[u8]) -> Option<(u32, u16, u16)> {
    if b.len() < MESSAGE_HEADER_LEN || !(DG_MESSAGE..=DG_ECHO).contains(&b[0]) {
        return None;
    }
    Some((
        u32::from_be_bytes(b[1..5].try_into().unwrap()),
        u16::from_be_bytes(b[5..7].try_into().unwrap()),
        u16::from_be_bytes(b[7..9].try_into().unwrap()),
    ))
}

/// Rewrites a channel datagram's `sid` and `broadcast` in place.
pub fn set_route(b: &mut [u8], sid: u32, broadcast: u16) {
    b[1..5].copy_from_slice(&sid.to_be_bytes());
    b[5..7].copy_from_slice(&broadcast.to_be_bytes());
}

/// Rewrites a track frame's `broadcast` and `relay_us` in place.
pub fn set_track_frame_route(b: &mut [u8], broadcast: u16, relay_us: u64) {
    b[1..3].copy_from_slice(&broadcast.to_be_bytes());
    b[26..34].copy_from_slice(&relay_us.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let all = [
            Datagram::Message {
                sid: 7,
                broadcast: 1,
                channel: 2,
                seq: 9,
                sent_us: 123,
                payload: Bytes::from_static(br#"{"v":1}"#),
            },
            Datagram::Ack {
                sid: 7,
                broadcast: 1,
                channel: 2,
                seq: 9,
                echo_us: 123,
                status: AckStatus::Superseded,
                detail: "old".into(),
            },
            Datagram::Heartbeat {
                sid: 0,
                broadcast: 3,
                lock: 0,
                seq: 1,
                sent_us: 5,
            },
            Datagram::Echo {
                sid: 4,
                broadcast: 3,
                lock: 0,
                seq: 1,
                echo_us: 5,
                flags: ECHO_TRIPPED,
            },
            Datagram::TrackFrame {
                broadcast: 2,
                track: 5,
                seq: 77,
                flags: 1,
                capture_us: 10,
                publisher_us: 11,
                relay_us: 12,
                payload: Bytes::from_static(b"imu"),
            },
        ];
        for d in all {
            assert_eq!(Datagram::decode(d.encode()).unwrap(), d);
        }
        assert!(Datagram::decode(Bytes::from_static(&[0x20, 0])).is_err());
        assert!(Datagram::decode(Bytes::from_static(&[0x10; 30])).is_err());
    }

    #[test]
    fn route_in_place() {
        let d = Datagram::Message {
            sid: 0,
            broadcast: 9,
            channel: 2,
            seq: 1,
            sent_us: 1,
            payload: Bytes::new(),
        };
        let mut b = d.encode().to_vec();
        assert_eq!(peek_route(&b), Some((0, 9, 2)));
        set_route(&mut b, 42, 0);
        assert_eq!(peek_route(&b), Some((42, 0, 2)));

        let f = Datagram::TrackFrame {
            broadcast: 1,
            track: 2,
            seq: 3,
            flags: 0,
            capture_us: 4,
            publisher_us: 5,
            relay_us: 0,
            payload: Bytes::from_static(b"x"),
        };
        let mut b = f.encode().to_vec();
        set_track_frame_route(&mut b, 8, 99);
        let Datagram::TrackFrame {
            broadcast, relay_us, ..
        } = Datagram::decode(Bytes::from(b)).unwrap()
        else {
            panic!()
        };
        assert_eq!((broadcast, relay_us), (8, 99));
    }
}
