//! Control stream messages: `len:u32` (big endian) followed by one JSON
//! object with a `type` field.

use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{BuildInfo, WireError};

/// Largest control message accepted.
pub const MAX_CONTROL_LEN: usize = 256 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Publisher,
    Operator,
    Viewer,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Role::Publisher => "publisher",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        })
    }
}

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "publisher" => Ok(Role::Publisher),
            "operator" => Ok(Role::Operator),
            "viewer" => Ok(Role::Viewer),
            other => Err(format!("unknown role '{other}' (publisher, operator or viewer)")),
        }
    }
}

/// A camera as announced by the publisher. Its position in the announce
/// list is the `camera` index used on group streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraInfo {
    pub id: String,
    pub label: String,
    /// `h264` or `h265`.
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Live per-camera numbers reported by the publisher.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CameraStatus {
    pub id: String,
    /// The TCP source is connected and producing frames.
    pub online: bool,
    pub codec_string: Option<String>,
    pub frames: u64,
    pub groups: u64,
    pub fps: f64,
    pub kbps: f64,
    /// Frames dropped on the publisher -> relay hop (late or superseded).
    pub dropped: u64,
    /// Mean capture -> publisher latency over the last report, ms (SEI only).
    pub capture_to_publisher_ms: Option<f64>,
}

/// The robot-side teleop / dead-man state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeleopStatus {
    /// Safe-stopped; stays tripped until the next accepted command.
    pub deadman_tripped: bool,
    pub reason: String,
    pub last_seq: u32,
    pub commands: u64,
    /// Session id of the operator the publisher currently accepts.
    pub operator: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublisherStatus {
    pub publisher_id: String,
    /// The publisher's build; `None` before protocol 1.1.
    #[serde(default)]
    pub build: Option<BuildInfo>,
    pub cameras: Vec<CameraInfo>,
    pub camera_status: Vec<CameraStatus>,
    pub teleop: TeleopStatus,
    /// Relay <-> publisher round trip (control stream ping), ms.
    pub rtt_ms: Option<f64>,
    /// Publisher clock minus relay clock, microseconds.
    pub clock_offset_us: Option<i64>,
    /// Bound on the offset's error: half the best round trip it came from.
    #[serde(default)]
    pub clock_error_us: Option<i64>,
    /// QUIC path stats of the publisher -> relay connection.
    pub quic: Option<QuicStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuicStats {
    pub rtt_ms: f64,
    pub cwnd: u64,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_events: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    // publisher -> relay
    Announce {
        room_id: String,
        publisher_id: String,
        cameras: Vec<CameraInfo>,
        /// `None` from publishers before protocol 1.1.
        #[serde(default)]
        build: Option<BuildInfo>,
    },
    PublisherStatus {
        cameras: Vec<CameraStatus>,
        teleop: TeleopStatus,
    },

    // relay -> publisher
    AnnounceOk {
        relay_us: u64,
        #[serde(default)]
        relay: Option<BuildInfo>,
    },
    /// The publisher's current operator changed (teleop sequence restarts).
    Operator {
        session: Option<String>,
    },

    // relay -> client. The room itself (state and events) arrives on the
    // events stream; see [`crate::room`].
    Welcome {
        session: String,
        role: Role,
        #[serde(default)]
        relay: Option<BuildInfo>,
    },
    ClaimResult {
        ok: bool,
        holder: Option<String>,
        reason: Option<String>,
    },
    /// QUIC stats of the relay -> this browser connection.
    LinkStats {
        quic: QuicStats,
    },

    // client -> relay
    /// The client's build. Must be the first message on the control
    /// stream; the relay joins the client to the room only after it.
    Hello {
        build: BuildInfo,
    },
    Claim {
        force: bool,
    },
    Release,

    // browser -> relay -> publisher
    KeyframeRequest {
        camera: String,
        reason: String,
    },

    // either direction: clock offset and RTT
    Ping {
        id: u64,
        t0_us: u64,
    },
    Pong {
        id: u64,
        t0_us: u64,
        t1_us: u64,
    },

    Error {
        message: String,
    },

    /// Any message type this build does not know: from a newer minor of the
    /// protocol. Receivers ignore it. Never sent.
    #[serde(other)]
    Unknown,
}

impl Control {
    /// Appends the length-prefixed JSON encoding.
    pub fn encode(&self, buf: &mut BytesMut) {
        let json = serde_json::to_vec(self).expect("control messages serialize");
        buf.put_u32(json.len() as u32);
        buf.put_slice(&json);
    }

    pub fn check_len(len: u32) -> Result<usize, WireError> {
        let len = len as usize;
        if len == 0 || len > MAX_CONTROL_LEN {
            return Err(WireError::FrameLen(len));
        }
        Ok(len)
    }

    pub fn decode(json: &[u8]) -> Result<Self, WireError> {
        serde_json::from_slice(json).map_err(|e| WireError::Json(e.to_string()))
    }
}

/// NTP-style offset of the remote clock: `t1 - (t0 + t3) / 2`, in
/// microseconds, and the round trip `t3 - t0`.
pub fn clock_offset(t0_us: u64, t1_us: u64, t3_us: u64) -> (i64, u64) {
    let rtt = t3_us.saturating_sub(t0_us);
    let mid = (t0_us as i128 + t3_us as i128) / 2;
    ((t1_us as i128 - mid) as i64, rtt)
}

/// Clock offset estimate from a window of ping samples.
///
/// Each sample's offset is exact only if the request and the reply took
/// equally long; queueing on one direction (a Starlink uplink full of
/// video) biases it by up to half the round trip. The sample with the
/// smallest round trip had the least queueing, so its offset is the best
/// one in the window, and half its round trip bounds the error (the NTP
/// clock filter idea).
#[derive(Debug, Clone)]
pub struct ClockFilter {
    samples: std::collections::VecDeque<(i64, u64)>,
    window: usize,
}

impl ClockFilter {
    pub fn new(window: usize) -> Self {
        Self { samples: std::collections::VecDeque::with_capacity(window), window }
    }

    pub fn add(&mut self, offset_us: i64, rtt_us: u64) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back((offset_us, rtt_us));
    }

    /// `(offset_us, error_us)` of the best sample in the window.
    pub fn estimate(&self) -> Option<(i64, i64)> {
        self.samples.iter().min_by_key(|(_, rtt)| *rtt).map(|&(off, rtt)| (off, (rtt / 2) as i64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_filter_prefers_least_queued_sample() {
        let mut f = ClockFilter::new(3);
        assert_eq!(f.estimate(), None);
        f.add(5_000, 60_000); // queued
        f.add(100, 2_000);
        f.add(-3_000, 40_000);
        assert_eq!(f.estimate(), Some((100, 1_000)));
        f.add(7_000, 50_000); // pushes the best sample out
        f.add(8_000, 50_000);
        assert_eq!(f.estimate(), Some((-3_000, 20_000)));
    }

    #[test]
    fn control_roundtrip_and_tags() {
        let m = Control::KeyframeRequest {
            camera: "front".into(),
            reason: "late".into(),
        };
        let mut b = BytesMut::new();
        m.encode(&mut b);
        let len = Control::check_len(u32::from_be_bytes(b[..4].try_into().unwrap())).unwrap();
        let json = &b[4..4 + len];
        assert!(std::str::from_utf8(json)
            .unwrap()
            .contains(r#""type":"keyframe_request""#));
        assert_eq!(Control::decode(json).unwrap(), m);
        assert_eq!(Control::decode(br#"{"type":"release"}"#).unwrap(), Control::Release);
    }

    #[test]
    fn newer_messages_and_fields_are_tolerated() {
        // A message type from a later minor.
        assert_eq!(
            Control::decode(br#"{"type":"from_the_future","x":1}"#).unwrap(),
            Control::Unknown
        );
        // An extra field on a known message.
        assert_eq!(
            Control::decode(br#"{"type":"claim","force":true,"priority":3}"#).unwrap(),
            Control::Claim { force: true }
        );
        // A 1.0 announce, without build info.
        let m = Control::decode(br#"{"type":"announce","room_id":"a","publisher_id":"p","cameras":[]}"#).unwrap();
        assert!(matches!(m, Control::Announce { build: None, .. }));
    }

    #[test]
    fn offset() {
        // Remote is 1000 us ahead; 200 us each way.
        assert_eq!(clock_offset(10_000, 11_200, 10_400), (1000, 400));
    }
}
