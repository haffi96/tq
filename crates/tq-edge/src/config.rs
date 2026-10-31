//! tq-edge's config: where the relay is, and the broadcast to publish: its
//! tracks (each fed by a source), its channels (each feeding a sink), and
//! optionally teleop. See `config/edge.example.json`.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use tq_transport::quic::QuicOptions;
use tq_wire::{Advertisement, ChannelInfo, DataInfo, Delivery, TrackInfo, TrackKind, VideoInfo};

use crate::{sink::SinkConfig, source::SourceConfig};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The room to publish into.
    pub room: String,
    /// The broadcast's name in the room. Default: the room.
    #[serde(default)]
    pub broadcast: String,
    pub relay: Relay,
    /// How often `.status` goes out when nothing changes.
    #[serde(default = "default_status_interval")]
    pub status_interval_ms: u64,
    #[serde(default)]
    pub tracks: Vec<TrackConfig>,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    /// Drive and e-stop channels behind a dead-man lock (`tq-teleop`).
    #[serde(default)]
    pub teleop: Option<TeleopConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relay {
    /// `https://host:port` of the relay's WebTransport endpoint.
    pub url: String,
    /// How to trust the relay's certificate.
    #[serde(default)]
    pub trust: Trust,
    /// Signs a token per connection. `TQ_SHARED_SECRET` overrides.
    #[serde(default)]
    pub shared_secret: String,
    /// A fixed token instead. `TQ_TOKEN` overrides.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub quic: QuicOptions,
    /// Uplink drop policy: a frame older than this since it was ready is
    /// dropped with the rest of its group.
    #[serde(default = "default_max_lag")]
    pub max_lag_ms: u64,
}

fn default_max_lag() -> u64 {
    400
}
fn default_status_interval() -> u64 {
    1000
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "mode", deny_unknown_fields)]
pub enum Trust {
    /// Fetch the relay's self-signed certificate hash from its HTTP API
    /// (`GET <url>` returning `{"sha256": "<hex>"}`) on every connect.
    HashUrl { url: String },
    /// Pin a fixed SHA-256 of the relay certificate.
    Hash { sha256: String },
    /// The platform's CA roots (real certificate).
    #[default]
    SystemRoots,
    /// Skip verification. Development only.
    Insecure,
}

/// One track and the source that feeds it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackConfig {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// 0–255, higher first. Default 128 for video, 96 for data.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Free-form, for apps (units, ranges, field descriptions).
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    /// `video` or `data`.
    pub kind: TrackKind,
    /// For video tracks.
    #[serde(default)]
    pub video: Option<VideoSize>,
    /// For data tracks: required.
    #[serde(default)]
    pub data: Option<DataConfig>,
    pub source: SourceConfig,
}

/// What a video track says about itself; the codec comes from the source.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSize {
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataConfig {
    /// `json` (default), `cbor` or `bytes`.
    #[serde(default = "json")]
    pub encoding: String,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default)]
    pub rate_hz: Option<f64>,
    #[serde(default)]
    pub window_ms: Option<u32>,
}

fn json() -> String {
    "json".into()
}

/// One channel and the sink its messages go to.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// A lock that makes the channel exclusive: `drive`, with teleop.
    #[serde(default)]
    pub lock: Option<String>,
    /// Free-form, for apps.
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    pub sink: SinkConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeleopConfig {
    /// Trip the dead-man when the holder's heartbeats stop for this long.
    #[serde(default = "default_watchdog_ms")]
    pub heartbeat_loss_ms: u64,
    /// Once commands have started, trip when they stop for this long.
    #[serde(default = "default_watchdog_ms")]
    pub command_stale_ms: u64,
    /// Where accepted drive commands and safe-stops go.
    #[serde(default)]
    pub sink: SinkConfig,
}

fn default_watchdog_ms() -> u64 {
    500
}

impl TeleopConfig {
    pub fn watchdog(&self) -> tq_teleop::Config {
        tq_teleop::Config {
            heartbeat_loss_ms: self.heartbeat_loss_ms,
            command_stale_ms: self.command_stale_ms,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        // Empty counts as unset: the installed edge.env has an empty
        // `TQ_SHARED_SECRET=` for the operator to fill in.
        let env = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        if let Some(s) = env("TQ_SHARED_SECRET") {
            cfg.relay.shared_secret = s;
        }
        if let Some(t) = env("TQ_TOKEN") {
            cfg.relay.token = t;
        }
        if cfg.broadcast.is_empty() {
            cfg.broadcast = cfg.room.clone();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// The broadcast this config publishes.
    pub fn advertisement(&self) -> Advertisement {
        let mut ad = Advertisement {
            broadcast: self.broadcast.clone(),
            ..Default::default()
        };
        for t in &self.tracks {
            let mut info = match (t.kind, &t.data) {
                (TrackKind::Data, Some(data)) => TrackInfo::data(
                    &t.id,
                    DataInfo {
                        encoding: data.encoding.clone(),
                        delivery: data.delivery,
                        rate_hz: data.rate_hz,
                        window_ms: data.window_ms,
                    },
                ),
                _ => {
                    let size = t.video.clone().unwrap_or_default();
                    TrackInfo::video(
                        &t.id,
                        VideoInfo {
                            codec: t.source.codec().unwrap_or("auto").to_string(),
                            width: size.width,
                            height: size.height,
                            fps: size.fps,
                        },
                    )
                }
            };
            info.label = if t.label.is_empty() { t.id.clone() } else { t.label.clone() };
            info.priority = t.priority;
            info.meta = t.meta.clone();
            ad.tracks.push(info);
        }
        for c in &self.channels {
            ad.channels.push(ChannelInfo {
                id: c.id.clone(),
                label: c.label.clone(),
                lock: c.lock.clone(),
                encoding: "json".into(),
                schema: c.schema.clone(),
            });
        }
        if let Some(t) = &self.teleop {
            tq_teleop::advertise(&mut ad, &t.watchdog());
        } else {
            // `.status` goes out either way.
            tq_teleop::advertise_status(&mut ad);
        }
        ad
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        if self.room.is_empty() || !self.room.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            problems.push("room must be non-empty [A-Za-z0-9_-]".to_string());
        }
        if url::Url::parse(&self.relay.url)
            .map(|u| u.scheme() != "https")
            .unwrap_or(true)
        {
            problems.push(format!("relay.url '{}' must be an https:// URL", self.relay.url));
        }
        if self.relay.shared_secret.is_empty() && self.relay.token.is_empty() {
            problems.push("set relay.shared_secret (or TQ_SHARED_SECRET) or relay.token (or TQ_TOKEN)".into());
        }
        if self.relay.max_lag_ms == 0 {
            problems.push("relay.max_lag_ms must be > 0".into());
        }
        if self.status_interval_ms == 0 {
            problems.push("status_interval_ms must be > 0".into());
        }
        if self.tracks.is_empty() && self.channels.is_empty() && self.teleop.is_none() {
            problems.push("nothing to publish: add tracks, channels or teleop".into());
        }
        for t in &self.tracks {
            if t.id.starts_with('.') {
                problems.push(format!("track '{}': ids starting with '.' are reserved", t.id));
            }
            match (t.kind, &t.video, &t.data) {
                (TrackKind::Video, _, None) | (TrackKind::Data, None, Some(_)) => {}
                (TrackKind::Video, _, Some(_)) => problems.push(format!("track '{}': `data` is for data tracks", t.id)),
                (TrackKind::Data, Some(_), _) => problems.push(format!("track '{}': `video` is for video tracks", t.id)),
                (TrackKind::Data, None, None) => problems.push(format!("track '{}': a data track needs `data`", t.id)),
            }
            if let Err(e) = t.source.check(t.kind) {
                problems.push(format!("track '{}': {e}", t.id));
            }
        }
        if let Err(e) = self.advertisement().validate() {
            problems.push(e.to_string());
        }
        anyhow::ensure!(problems.is_empty(), "config: {}", problems.join("; "));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every config in `config/` parses, validates, and makes a valid
    /// broadcast.
    #[test]
    fn example_configs_are_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !(name.starts_with("edge") && name.ends_with(".json")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let mut cfg: Config = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
            // Deployed configs get their secret from the environment.
            if cfg.relay.shared_secret.is_empty() && cfg.relay.token.is_empty() {
                cfg.relay.shared_secret = "x".into();
            }
            if cfg.broadcast.is_empty() {
                cfg.broadcast = cfg.room.clone();
            }
            cfg.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            n += 1;
        }
        assert!(n >= 3, "found {n} configs");
    }

    #[test]
    fn mistakes_are_reported() {
        let base = serde_json::json!({
            "room": "r1",
            "relay": { "url": "https://relay:443", "shared_secret": "s" },
            "tracks": [{ "id": "t", "kind": "data", "source": { "type": "tcp", "addr": "h:1" } }],
        });
        let check = |v: serde_json::Value| {
            let mut cfg: Config = serde_json::from_value(v).map_err(|e| e.to_string())?;
            cfg.broadcast = cfg.room.clone();
            cfg.validate().map_err(|e| e.to_string())
        };
        assert!(check(base.clone()).unwrap_err().contains("needs `data`"));
        let mut v = base.clone();
        v["tracks"][0]["kind"] = "video".into();
        v["tracks"][0]["source"]["framing"] = "lines".into();
        assert!(check(v).unwrap_err().contains("unknown video framing"));
        let mut v = base.clone();
        v["tracks"][0]["typo"] = 1.into();
        assert!(check(v).unwrap_err().contains("unknown field"));
        let mut v = base;
        v["tracks"][0]["data"] = serde_json::json!({ "delivery": "latest" });
        v["channels"] = serde_json::json!([{ "id": "go", "lock": "drive", "sink": { "type": "log" } }]);
        assert!(check(v).unwrap_err().contains("drive"));
    }
}
