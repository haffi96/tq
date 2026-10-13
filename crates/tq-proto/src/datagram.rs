//! Teleop datagrams. Browser -> relay -> publisher for commands and
//! heartbeats, publisher -> relay -> browser for acks and echoes. The relay
//! forwards them unchanged, and only from the session holding the operator
//! claim.
//!
//! ```text
//! 0x10 command    seq:u32 sent_us:u64 body:utf8 json {"command": "...", "args": {...}}
//! 0x11 ack        seq:u32 echo_us:u64 status:u8 detail:utf8
//! 0x12 heartbeat  seq:u32 sent_us:u64
//! 0x13 echo       seq:u32 echo_us:u64 tripped:u8
//! ```
//!
//! `sent_us` is on the sender's clock and only ever compared with itself
//! (echoed back), so no clock sync is needed for RTT.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::WireError;

pub const DG_COMMAND: u8 = 0x10;
pub const DG_ACK: u8 = 0x11;
pub const DG_HEARTBEAT: u8 = 0x12;
pub const DG_ECHO: u8 = 0x13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AckStatus {
    Ok = 0,
    /// An equal or newer sequence number was already applied.
    Superseded = 1,
    /// The sink refused the command.
    Rejected = 2,
    /// Command body was not valid JSON.
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
    Command {
        seq: u32,
        sent_us: u64,
        body: Bytes,
    },
    Ack {
        seq: u32,
        echo_us: u64,
        status: AckStatus,
        detail: String,
    },
    Heartbeat {
        seq: u32,
        sent_us: u64,
    },
    Echo {
        seq: u32,
        echo_us: u64,
        tripped: bool,
    },
}

impl Datagram {
    pub fn encode(&self) -> Bytes {
        let mut b = BytesMut::with_capacity(32);
        match self {
            Datagram::Command { seq, sent_us, body } => {
                b.put_u8(DG_COMMAND);
                b.put_u32(*seq);
                b.put_u64(*sent_us);
                b.put_slice(body);
            }
            Datagram::Ack {
                seq,
                echo_us,
                status,
                detail,
            } => {
                b.put_u8(DG_ACK);
                b.put_u32(*seq);
                b.put_u64(*echo_us);
                b.put_u8(*status as u8);
                b.put_slice(detail.as_bytes());
            }
            Datagram::Heartbeat { seq, sent_us } => {
                b.put_u8(DG_HEARTBEAT);
                b.put_u32(*seq);
                b.put_u64(*sent_us);
            }
            Datagram::Echo { seq, echo_us, tripped } => {
                b.put_u8(DG_ECHO);
                b.put_u32(*seq);
                b.put_u64(*echo_us);
                b.put_u8(*tripped as u8);
            }
        }
        b.freeze()
    }

    pub fn decode(mut b: Bytes) -> Result<Self, WireError> {
        if b.len() < 13 {
            return Err(WireError::Truncated);
        }
        let ty = b.get_u8();
        let seq = b.get_u32();
        let t = b.get_u64();
        Ok(match ty {
            DG_COMMAND => Datagram::Command {
                seq,
                sent_us: t,
                body: b,
            },
            DG_ACK => {
                if b.is_empty() {
                    return Err(WireError::Truncated);
                }
                let status = AckStatus::from_u8(b.get_u8());
                let detail = String::from_utf8_lossy(&b).into_owned();
                Datagram::Ack {
                    seq,
                    echo_us: t,
                    status,
                    detail,
                }
            }
            DG_HEARTBEAT => Datagram::Heartbeat { seq, sent_us: t },
            DG_ECHO => Datagram::Echo {
                seq,
                echo_us: t,
                tripped: b.first().copied().unwrap_or(0) != 0,
            },
            other => return Err(WireError::DatagramType(other)),
        })
    }

    /// Datagram type byte without decoding the rest.
    pub fn peek_type(b: &[u8]) -> Option<u8> {
        b.first().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let all = [
            Datagram::Command {
                seq: 9,
                sent_us: 123,
                body: Bytes::from_static(br#"{"command":"drive"}"#),
            },
            Datagram::Ack {
                seq: 9,
                echo_us: 123,
                status: AckStatus::Superseded,
                detail: "old".into(),
            },
            Datagram::Heartbeat { seq: 1, sent_us: 5 },
            Datagram::Echo {
                seq: 1,
                echo_us: 5,
                tripped: true,
            },
        ];
        for d in all {
            assert_eq!(Datagram::decode(d.encode()).unwrap(), d);
        }
        assert!(Datagram::decode(Bytes::from_static(&[0x10, 0])).is_err());
    }
}
