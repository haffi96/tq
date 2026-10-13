use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use tq_media::{Codec, Framing, FramingMode};
use tq_transport::quic::QuicOptions;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub av_id: String,
    #[serde(default)]
    pub publisher_id: String,
    /// `https://host:port` of the relay's WebTransport endpoint.
    pub relay_url: String,
    /// How to trust the relay certificate; see [`Trust`].
    #[serde(default)]
    pub relay_trust: Trust,
    /// Mints a publisher token per connection. `TQ_SHARED_SECRET` overrides.
    #[serde(default)]
    pub shared_secret: String,
    /// Fixed token instead of minting one. `TQ_TOKEN` overrides.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub quic: QuicOptions,
    /// Uplink drop policy: a frame older than this since it was assembled
    /// is dropped with the rest of its GOP.
    #[serde(default = "default_max_lag")]
    pub max_lag_ms: u64,
    #[serde(default = "default_status_interval")]
    pub status_interval_ms: u64,
    #[serde(default)]
    pub teleop: TeleopConfig,
    pub cameras: Vec<CameraConfig>,
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
    /// Plain HTTP: development and VPNs only.
    HashUrl { url: String },
    /// Pin a fixed SHA-256 of the relay certificate.
    Hash { sha256: String },
    /// The platform's CA roots (real certificate).
    #[default]
    SystemRoots,
    /// Skip verification. Development only.
    Insecure,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeleopConfig {
    /// Trip the dead-man when no operator heartbeat arrives for this long.
    pub heartbeat_loss_ms: u64,
    /// Once commands have started, trip when they stop for this long.
    pub command_stale_ms: u64,
}

impl Default for TeleopConfig {
    fn default() -> Self {
        Self {
            heartbeat_loss_ms: 500,
            command_stale_ms: 500,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// `host:port` of the encoder's TCP server.
    pub source: String,
    /// `h264`, `h265` or `auto` (detected from the first NAL unit).
    #[serde(default = "auto")]
    pub codec: String,
    /// `annexb`, `length_prefixed` or `auto` (detected from the first bytes).
    #[serde(default = "auto")]
    pub framing: String,
    /// Complete an access unit as soon as its slice arrives. Only for
    /// single-slice encoders that do not terminate access units with an AUD;
    /// saves one frame interval of latency there.
    #[serde(default)]
    pub complete_on_vcl: bool,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: u32,
}

fn auto() -> String {
    "auto".into()
}

impl CameraConfig {
    pub fn codec(&self) -> anyhow::Result<Option<Codec>> {
        if self.codec == "auto" {
            return Ok(None);
        }
        self.codec.parse().map(Some).map_err(anyhow::Error::msg)
    }

    pub fn framing(&self) -> anyhow::Result<FramingMode> {
        Ok(match self.framing.as_str() {
            "auto" => FramingMode::Auto,
            "annexb" | "byte-stream" => FramingMode::Fixed(Framing::AnnexB),
            "length_prefixed" | "avc" => FramingMode::Fixed(Framing::LengthPrefixed),
            other => anyhow::bail!(
                "camera {}: unknown framing '{other}' (auto, annexb, length_prefixed)",
                self.id
            ),
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Config = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if let Ok(s) = std::env::var("TQ_SHARED_SECRET") {
            cfg.shared_secret = s;
        }
        if let Ok(t) = std::env::var("TQ_TOKEN") {
            cfg.token = t;
        }
        if cfg.publisher_id.is_empty() {
            cfg.publisher_id = format!("{}-publisher", cfg.av_id);
        }
        for c in &mut cfg.cameras {
            if c.label.is_empty() {
                c.label = c.id.clone();
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        if self.av_id.is_empty()
            || !self
                .av_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            problems.push("av_id must be non-empty [A-Za-z0-9_-]".to_string());
        }
        if url::Url::parse(&self.relay_url)
            .map(|u| u.scheme() != "https")
            .unwrap_or(true)
        {
            problems.push(format!("relay_url '{}' must be an https:// URL", self.relay_url));
        }
        if self.shared_secret.is_empty() && self.token.is_empty() {
            problems.push("set shared_secret (or TQ_SHARED_SECRET) or token (or TQ_TOKEN)".into());
        }
        if self.cameras.is_empty() {
            problems.push("at least one camera is required".into());
        }
        let mut ids = std::collections::HashSet::new();
        for c in &self.cameras {
            if c.id.is_empty() || !ids.insert(&c.id) {
                problems.push(format!("camera id '{}' is empty or duplicated", c.id));
            }
            if let Err(e) = c.codec() {
                problems.push(format!("camera {}: {e}", c.id));
            }
            if let Err(e) = c.framing() {
                problems.push(e.to_string());
            }
        }
        if self.max_lag_ms == 0 {
            problems.push("max_lag_ms must be > 0".into());
        }
        anyhow::ensure!(problems.is_empty(), "config: {}", problems.join("; "));
        Ok(())
    }
}
