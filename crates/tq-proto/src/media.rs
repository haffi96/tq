//! Group (GOP) streams.
//!
//! ```text
//! group stream := group_header frame*
//! group_header := type:u8 (=0x01) version:u8 (=1) camera:u16 group:u64
//!                 codec_len:u8 codec:utf8[codec_len]
//! frame        := len:u32 (bytes after this field)
//!                 index:u32 flags:u8 capture_us:u64 publisher_us:u64
//!                 relay_us:u64 frame_id:u32 payload
//! ```
//!
//! All integers are big endian. `index` is the frame's position in the
//! group (0 = the keyframe). Times are Unix microseconds: `capture_us` on
//! the camera machine's clock (from the encoder's LKTS SEI, or the publisher
//! receive time when there is none), `publisher_us` when the publisher had
//! the complete access unit, `relay_us` when the relay had it (0 on the
//! publisher -> relay hop). The payload is one Annex B access unit; group
//! keyframes carry their parameter sets.

use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const STREAM_TYPE_GROUP: u8 = 0x01;
pub const VERSION: u8 = 1;
pub const GROUP_HEADER_FIXED: usize = 1 + 1 + 2 + 8 + 1;
pub const FRAME_HEADER_LEN: usize = 4 + 1 + 8 + 8 + 8 + 4;
/// Largest frame accepted from the wire.
pub const MAX_FRAME_LEN: usize = 16 << 20;

pub const FLAG_KEY: u8 = 1 << 0;
/// `capture_us` came from the encoder's SEI rather than the publisher clock.
pub const FLAG_CAPTURE_FROM_SEI: u8 = 1 << 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("unknown stream type {0:#04x}")]
    StreamType(u8),
    #[error("unsupported version {0}")]
    Version(u8),
    #[error("frame length {0} out of range")]
    FrameLen(usize),
    #[error("codec string is not UTF-8")]
    Codec,
    #[error("message truncated")]
    Truncated,
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("unknown datagram type {0:#04x}")]
    DatagramType(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHeader {
    /// Index of the camera in the publisher's announce.
    pub camera: u16,
    /// GOP sequence number, per camera, increasing.
    pub group: u64,
    /// WebCodecs codec string of the group, e.g. `avc1.42e01f`.
    pub codec: String,
}

impl GroupHeader {
    pub fn encode(&self, buf: &mut BytesMut) {
        let codec = self.codec.as_bytes();
        let codec = &codec[..codec.len().min(255)];
        buf.put_u8(STREAM_TYPE_GROUP);
        buf.put_u8(VERSION);
        buf.put_u16(self.camera);
        buf.put_u64(self.group);
        buf.put_u8(codec.len() as u8);
        buf.put_slice(codec);
    }

    /// Parses the fixed part; returns the header with an empty codec and the
    /// codec string length still to read.
    pub fn decode_fixed(mut b: &[u8]) -> Result<(Self, usize), WireError> {
        if b.len() < GROUP_HEADER_FIXED {
            return Err(WireError::Truncated);
        }
        let ty = b.get_u8();
        if ty != STREAM_TYPE_GROUP {
            return Err(WireError::StreamType(ty));
        }
        let version = b.get_u8();
        if version != VERSION {
            return Err(WireError::Version(version));
        }
        let camera = b.get_u16();
        let group = b.get_u64();
        let codec_len = b.get_u8() as usize;
        Ok((
            Self {
                camera,
                group,
                codec: String::new(),
            },
            codec_len,
        ))
    }

    pub fn decode(b: &[u8]) -> Result<(Self, usize), WireError> {
        let (mut h, codec_len) = Self::decode_fixed(b)?;
        let end = GROUP_HEADER_FIXED + codec_len;
        let codec = b.get(GROUP_HEADER_FIXED..end).ok_or(WireError::Truncated)?;
        h.codec = std::str::from_utf8(codec).map_err(|_| WireError::Codec)?.to_owned();
        Ok((h, end))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameHeader {
    pub index: u32,
    pub flags: u8,
    pub capture_us: u64,
    pub publisher_us: u64,
    pub relay_us: u64,
    pub frame_id: u32,
}

impl FrameHeader {
    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }

    /// Writes the length prefix and header for a payload of `payload_len`.
    pub fn encode(&self, payload_len: usize, buf: &mut BytesMut) {
        buf.put_u32((FRAME_HEADER_LEN + payload_len) as u32);
        buf.put_u32(self.index);
        buf.put_u8(self.flags);
        buf.put_u64(self.capture_us);
        buf.put_u64(self.publisher_us);
        buf.put_u64(self.relay_us);
        buf.put_u32(self.frame_id);
    }

    /// Validates a frame length prefix.
    pub fn check_len(len: u32) -> Result<usize, WireError> {
        let len = len as usize;
        if !(FRAME_HEADER_LEN..=MAX_FRAME_LEN).contains(&len) {
            return Err(WireError::FrameLen(len));
        }
        Ok(len)
    }

    /// Parses a header from the bytes after the length prefix.
    pub fn decode(mut b: &[u8]) -> Result<Self, WireError> {
        if b.len() < FRAME_HEADER_LEN {
            return Err(WireError::Truncated);
        }
        Ok(Self {
            index: b.get_u32(),
            flags: b.get_u8(),
            capture_us: b.get_u64(),
            publisher_us: b.get_u64(),
            relay_us: b.get_u64(),
            frame_id: b.get_u32(),
        })
    }
}

/// A frame as it moves through the publisher and relay.
#[derive(Debug, Clone)]
pub struct MediaFrame {
    pub camera: u16,
    pub group: u64,
    pub codec: std::sync::Arc<str>,
    pub header: FrameHeader,
    pub payload: Bytes,
}

impl MediaFrame {
    pub fn is_key(&self) -> bool {
        self.header.is_key()
    }

    pub fn group_header(&self) -> GroupHeader {
        GroupHeader {
            camera: self.camera,
            group: self.group,
            codec: self.codec.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_header_roundtrip() {
        let h = GroupHeader {
            camera: 3,
            group: 1 << 40,
            codec: "hev1.1.6.L93.B0".into(),
        };
        let mut b = BytesMut::new();
        h.encode(&mut b);
        let (d, n) = GroupHeader::decode(&b).unwrap();
        assert_eq!(d, h);
        assert_eq!(n, b.len());
    }

    #[test]
    fn frame_header_roundtrip() {
        let h = FrameHeader {
            index: 7,
            flags: FLAG_KEY,
            capture_us: 1,
            publisher_us: 2,
            relay_us: 3,
            frame_id: 4,
        };
        let mut b = BytesMut::new();
        h.encode(10, &mut b);
        let len = FrameHeader::check_len(u32::from_be_bytes(b[..4].try_into().unwrap())).unwrap();
        assert_eq!(len, FRAME_HEADER_LEN + 10);
        assert_eq!(FrameHeader::decode(&b[4..]).unwrap(), h);
    }

    #[test]
    fn rejects_bad_streams() {
        assert_eq!(GroupHeader::decode(&[0x02; 13]).unwrap_err(), WireError::StreamType(2));
        assert_eq!(FrameHeader::check_len(3).unwrap_err(), WireError::FrameLen(3));
    }
}
