//! Control stream messages: `len:u32` (big endian) followed by one JSON
//! object with a `type` field. Also what a broadcast is made of
//! ([`Advertisement`]), since that is what `advertise` carries.

use bytes::{BufMut, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{valid_name, BuildInfo, Path, Perms, WireError};

/// Largest control message accepted.
pub const MAX_CONTROL_LEN: usize = 256 << 10;

// --- broadcasts ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Video,
    Data,
}

/// How a data track's messages are grouped on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// One group per message; newer supersedes older. The relay keeps the
    /// newest for late joiners.
    #[default]
    Latest,
    /// Every message, in order, in groups of `window_ms`.
    Stream,
    /// One QUIC datagram per message ([`crate::Datagram::TrackFrame`]).
    Datagram,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    /// `h264`, `h265`, or `auto` until the source has said.
    pub codec: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataInfo {
    /// `json`, `cbor` or `bytes`; also the group header's codec.
    #[serde(default = "json")]
    pub encoding: String,
    #[serde(default)]
    pub delivery: Delivery,
    /// Nominal message rate, for apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<f64>,
    /// `stream` only: how long one group lasts. Default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<u32>,
}

fn json() -> String {
    "json".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub id: String,
    pub kind: TrackKind,
    #[serde(default)]
    pub label: String,
    /// 0–255, higher is sent first. Default 128 for video, 96 for data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataInfo>,
    /// Free-form, for apps (units, ranges, field descriptions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl TrackInfo {
    pub fn video(id: impl Into<String>, video: VideoInfo) -> Self {
        Self {
            id: id.into(),
            kind: TrackKind::Video,
            label: String::new(),
            priority: None,
            video: Some(video),
            data: None,
            meta: None,
        }
    }

    pub fn data(id: impl Into<String>, data: DataInfo) -> Self {
        Self {
            id: id.into(),
            kind: TrackKind::Data,
            label: String::new(),
            priority: None,
            video: None,
            data: Some(data),
            meta: None,
        }
    }

    pub fn priority(&self) -> u8 {
        self.priority.unwrap_or(match self.kind {
            TrackKind::Video => 128,
            TrackKind::Data => 96,
        })
    }

    /// `None` for video (groups are GOPs).
    pub fn delivery(&self) -> Option<Delivery> {
        self.data.as_ref().map(|d| d.delivery)
    }

    pub fn is_datagram(&self) -> bool {
        self.delivery() == Some(Delivery::Datagram)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watchdog {
    pub heartbeat_loss_ms: u64,
    pub message_stale_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// The lock that makes this channel exclusive, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
    #[serde(default = "json")]
    pub encoding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// Enforced by the owner; informational for everyone else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<Watchdog>,
}

/// A broadcast as its owner describes it. Indices on the wire are positions
/// in these lists.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Advertisement {
    pub broadcast: String,
    #[serde(default)]
    pub tracks: Vec<TrackInfo>,
    #[serde(default)]
    pub channels: Vec<ChannelInfo>,
    #[serde(default)]
    pub locks: Vec<LockInfo>,
}

impl Advertisement {
    /// Everything that makes the advertisement unusable, in one message.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        if !valid_name(&self.broadcast) {
            problems.push(format!("broadcast name '{}' is not valid", self.broadcast));
        }
        let mut names = std::collections::HashSet::new();
        for t in &self.tracks {
            if !valid_name(&t.id) || !names.insert(("track", &t.id)) {
                problems.push(format!("track id '{}' is invalid or duplicated", t.id));
            }
            match (t.kind, &t.video, &t.data) {
                (TrackKind::Video, Some(_), None) | (TrackKind::Data, None, Some(_)) => {}
                _ => problems.push(format!("track '{}': kind {:?} needs exactly its own info", t.id, t.kind)),
            }
        }
        for c in &self.channels {
            if !valid_name(&c.id) || !names.insert(("channel", &c.id)) {
                problems.push(format!("channel id '{}' is invalid or duplicated", c.id));
            }
            if let Some(l) = &c.lock {
                if !self.locks.iter().any(|x| &x.id == l) {
                    problems.push(format!("channel '{}': unknown lock '{l}'", c.id));
                }
            }
        }
        for l in &self.locks {
            if !valid_name(&l.id) || !names.insert(("lock", &l.id)) {
                problems.push(format!("lock id '{}' is invalid or duplicated", l.id));
            }
        }
        for (what, n) in [
            ("tracks", self.tracks.len()),
            ("channels", self.channels.len()),
            ("locks", self.locks.len()),
        ] {
            if n > u16::MAX as usize {
                problems.push(format!("too many {what}"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    pub fn track(&self, id: &str) -> Option<(u16, &TrackInfo)> {
        self.tracks
            .iter()
            .enumerate()
            .find(|(_, t)| t.id == id)
            .map(|(i, t)| (i as u16, t))
    }

    pub fn channel(&self, id: &str) -> Option<(u16, &ChannelInfo)> {
        self.channels
            .iter()
            .enumerate()
            .find(|(_, c)| c.id == id)
            .map(|(i, c)| (i as u16, c))
    }

    pub fn lock(&self, id: &str) -> Option<(u16, &LockInfo)> {
        self.locks
            .iter()
            .enumerate()
            .find(|(_, l)| l.id == id)
            .map(|(i, l)| (i as u16, l))
    }
}

// --- subscriptions -------------------------------------------------------

/// Which relay drop policy a subscription gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Drop aggressively for the lowest latency (driving).
    #[default]
    Realtime,
    /// Tolerate more lag for fewer gaps (watching).
    Smooth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "SubscriptionWire")]
pub struct Subscription {
    pub track: Path,
    /// `latest` and `datagram` tracks only: forward at most this often,
    /// always the newest message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rate_hz: Option<f64>,
    #[serde(default)]
    pub policy: Policy,
}

impl Subscription {
    pub fn new(track: Path) -> Self {
        Self {
            track,
            max_rate_hz: None,
            policy: Policy::default(),
        }
    }
}

impl From<Path> for Subscription {
    fn from(p: Path) -> Self {
        Subscription::new(p)
    }
}

/// A subscription may be written as just the path.
#[derive(Deserialize)]
#[serde(untagged)]
enum SubscriptionWire {
    Path(Path),
    Full {
        track: Path,
        #[serde(default)]
        max_rate_hz: Option<f64>,
        #[serde(default)]
        policy: Policy,
    },
}

impl From<SubscriptionWire> for Subscription {
    fn from(w: SubscriptionWire) -> Self {
        match w {
            SubscriptionWire::Path(track) => Subscription::new(track),
            SubscriptionWire::Full {
                track,
                max_rate_hz,
                policy,
            } => Subscription {
                track,
                max_rate_hz,
                policy,
            },
        }
    }
}

// --- stats ---------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuicStats {
    pub rtt_ms: f64,
    pub cwnd: u64,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_events: u64,
}

/// What only the owner knows about a track.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OwnerTrackStats {
    pub id: String,
    /// Frames dropped on the owner -> relay hop (late or superseded).
    pub dropped: u64,
}

// --- messages ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    // session -> relay, first
    /// The session's build. Must be the first message on the control
    /// stream; the relay joins the session to the room only after it.
    Hello { build: BuildInfo },
    // relay -> session, in reply
    Welcome {
        session: String,
        /// Routing id in datagrams.
        sid: u32,
        perms: Perms,
        relay: BuildInfo,
    },

    // publishing
    Advertise(Advertisement),
    AdvertiseOk {
        broadcast: String,
        /// The broadcast's index on this session's owner hop.
        index: u16,
        relay_us: u64,
    },
    AdvertiseError { broadcast: String, reason: String },
    Unadvertise { broadcast: String },
    /// Relay -> owner: how many sessions subscribe to one of its tracks.
    TrackDemand { track: Path, subscribers: u32 },
    /// Owner -> relay.
    TrackStats { broadcast: String, tracks: Vec<OwnerTrackStats> },

    // subscribing
    Subscribe { tracks: Vec<Subscription> },
    Unsubscribe { tracks: Vec<Path> },
    SubscribeResult {
        track: Path,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Subscriber -> relay -> owner (rate limited by the relay).
    KeyframeRequest { track: Path, reason: String },

    // locks
    Claim {
        lock: Path,
        #[serde(default)]
        force: bool,
    },
    Release { lock: Path },
    ClaimResult {
        lock: Path,
        ok: bool,
        holder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The claim this session now holds the lock under (see
        /// `LockHolder::claim`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<u64>,
    },
    /// Relay -> owner: who holds one of its locks now.
    LockHolder {
        lock: Path,
        sid: Option<u32>,
        session: Option<String>,
        /// Which claim: the relay numbers claims, so a new claim (even by
        /// the same session) differs from the relay restating the holder.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claim: Option<u64>,
    },

    /// Relay -> session: QUIC stats of its connection.
    LinkStats { quic: QuicStats },

    // either direction: clock offset and RTT
    Ping { id: u64, t0_us: u64 },
    Pong { id: u64, t0_us: u64, t1_us: u64 },

    Error { message: String },

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

#[cfg(test)]
mod tests {
    use super::*;

    fn rover() -> Advertisement {
        Advertisement {
            broadcast: "rover-7".into(),
            tracks: vec![
                TrackInfo::video(
                    "front",
                    VideoInfo {
                        codec: "h265".into(),
                        width: 1280,
                        height: 720,
                        fps: 30,
                    },
                ),
                TrackInfo::data(
                    "imu",
                    DataInfo {
                        encoding: "json".into(),
                        delivery: Delivery::Datagram,
                        rate_hz: Some(200.0),
                        window_ms: None,
                    },
                ),
            ],
            channels: vec![ChannelInfo {
                id: "drive".into(),
                label: String::new(),
                lock: Some("drive".into()),
                encoding: "json".into(),
                schema: None,
            }],
            locks: vec![LockInfo {
                id: "drive".into(),
                label: String::new(),
                watchdog: Some(Watchdog {
                    heartbeat_loss_ms: 500,
                    message_stale_ms: 500,
                }),
            }],
        }
    }

    #[test]
    fn control_roundtrip_and_tags() {
        let m = Control::KeyframeRequest {
            track: "rover-7/front".parse().unwrap(),
            reason: "late".into(),
        };
        let mut b = BytesMut::new();
        m.encode(&mut b);
        let len = Control::check_len(u32::from_be_bytes(b[..4].try_into().unwrap())).unwrap();
        let json = &b[4..4 + len];
        assert!(std::str::from_utf8(json)
            .unwrap()
            .contains(r#""type":"keyframe_request","track":"rover-7/front""#));
        assert_eq!(Control::decode(json).unwrap(), m);

        let ad = Control::Advertise(rover());
        let json = serde_json::to_string(&ad).unwrap();
        assert!(json.starts_with(r#"{"type":"advertise","broadcast":"rover-7","#), "{json}");
        assert_eq!(Control::decode(json.as_bytes()).unwrap(), ad);
    }

    #[test]
    fn subscriptions_take_a_path_or_an_object() {
        let m = Control::decode(
            br#"{"type":"subscribe","tracks":["a/front",{"track":"a/imu","max_rate_hz":60,"policy":"smooth"}]}"#,
        )
        .unwrap();
        let Control::Subscribe { tracks } = m else { panic!() };
        assert_eq!(tracks[0], Subscription::new("a/front".parse().unwrap()));
        assert_eq!(tracks[1].max_rate_hz, Some(60.0));
        assert_eq!(tracks[1].policy, Policy::Smooth);
        assert!(Control::decode(br#"{"type":"subscribe","tracks":["nope"]}"#).is_err());
    }

    #[test]
    fn newer_messages_and_fields_are_tolerated() {
        assert_eq!(
            Control::decode(br#"{"type":"from_the_future","x":1}"#).unwrap(),
            Control::Unknown
        );
        assert_eq!(
            Control::decode(br#"{"type":"release","lock":"a/b","priority":3}"#).unwrap(),
            Control::Release {
                lock: "a/b".parse().unwrap()
            }
        );
    }

    #[test]
    fn validation() {
        assert!(rover().validate().is_ok());
        let mut bad = rover();
        bad.channels[0].lock = Some("nope".into());
        bad.tracks.push(bad.tracks[0].clone());
        bad.tracks[1].video = Some(VideoInfo {
            codec: "h264".into(),
            width: 0,
            height: 0,
            fps: 0,
        });
        let e = bad.validate().unwrap_err();
        assert!(e.contains("unknown lock") && e.contains("duplicated") && e.contains("exactly"), "{e}");
        assert_eq!(rover().track("imu").map(|(i, _)| i), Some(1));
        assert!(rover().tracks[1].is_datagram());
        assert_eq!(rover().tracks[0].priority(), 128);
    }
}
