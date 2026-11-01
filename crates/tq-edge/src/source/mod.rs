//! Sources: what feeds each track. A source is a module with a `run`
//! function that produces into its track for as long as tq-edge runs,
//! reconnecting to whatever it reads from. Adding one is a module here and
//! a variant in [`SourceConfig`].
//!
//! Any program can be a source without Rust: anything that writes H.264 or
//! H.265 to a TCP socket is a video source, and anything that writes JSON
//! lines (or length-prefixed messages) is a data source.

mod tcp_data;
mod tcp_video;

use std::sync::{atomic::AtomicU64, Arc, Mutex};

use serde::Deserialize;
use tq::Publication;
use tq_wire::TrackKind;

use crate::config::TrackConfig;

/// Where a track's frames or messages come from, by `"type"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum SourceConfig {
    /// Connect to a TCP server and read from it.
    ///
    /// Video tracks: an H.264/H.265 byte stream, Annex B or
    /// length-prefixed. Data tracks: one message per line (`lines`, the
    /// default) or per `len:u32` big-endian prefix (`length_prefixed`).
    Tcp {
        /// `host:port`.
        addr: String,
        /// Video: `h264`, `h265` or `auto` (detected from the first NAL).
        #[serde(default)]
        codec: Option<String>,
        /// Video: `annexb`, `length_prefixed` or `auto`. Data: `lines` or
        /// `length_prefixed`.
        #[serde(default)]
        framing: Option<String>,
        /// Video: complete an access unit as soon as its slice arrives.
        /// Only for single-slice encoders that do not end access units
        /// with an AUD; saves a frame interval there.
        #[serde(default)]
        complete_on_vcl: bool,
    },
}

impl SourceConfig {
    /// The codec the config names for a video source, if not `auto`.
    pub fn codec(&self) -> Option<&str> {
        match self {
            SourceConfig::Tcp { codec, .. } => codec.as_deref().filter(|c| *c != "auto"),
        }
    }

    /// Whether this source can feed a track of `kind`.
    pub fn check(&self, kind: TrackKind) -> anyhow::Result<()> {
        match (self, kind) {
            (SourceConfig::Tcp { .. }, TrackKind::Video) => tcp_video::Options::from_config(self).map(|_| ()),
            (SourceConfig::Tcp { .. }, TrackKind::Data) => tcp_data::Framing::from_config(self).map(|_| ()),
        }
    }
}

/// What a source has produced, for the robot's `.status`.
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    /// The codec string, once a video source has seen one.
    pub codec: Mutex<Option<String>>,
}

/// Starts the source of `track`; it runs until the process ends.
pub fn spawn(track: &TrackConfig, p: Arc<Publication>, stats: Arc<Stats>) -> anyhow::Result<()> {
    let id = track.id.clone();
    match track.kind {
        TrackKind::Video => {
            let SourceConfig::Tcp { addr, .. } = &track.source;
            let opts = tcp_video::Options::from_config(&track.source)?;
            let out = p.video_with(&id, opts.video).expect("advertised from this config");
            tokio::spawn(tcp_video::run(addr.clone(), out, stats));
        }
        TrackKind::Data => {
            let SourceConfig::Tcp { addr, .. } = &track.source;
            let framing = tcp_data::Framing::from_config(&track.source)?;
            let encoding = track.data.as_ref().map_or("json", |d| d.encoding.as_str()).to_string();
            tokio::spawn(tcp_data::run(id, addr.clone(), framing, encoding, p, stats));
        }
    }
    Ok(())
}
