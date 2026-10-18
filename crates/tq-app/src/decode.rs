//! The platform's hardware decoder: VideoToolbox on macOS, NVDEC on Linux.
//! Both take Annex B access units and return a [`Picture`] the UI can draw.

use anyhow::Context;
use tq_media::Codec;

#[cfg(target_os = "linux")]
pub use crate::nvdec::{Decoder, Picture};
#[cfg(target_os = "macos")]
pub use crate::vt::{Decoder, Picture};

/// Codec of a group from its WebCodecs codec string.
pub fn codec_of(codec_string: &str) -> anyhow::Result<Codec> {
    if codec_string.starts_with("avc1") || codec_string.starts_with("avc3") {
        Ok(Codec::H264)
    } else if codec_string.starts_with("hev1") || codec_string.starts_with("hvc1") {
        Ok(Codec::H265)
    } else {
        Err(anyhow::anyhow!("unsupported codec {codec_string}")).context("codec")
    }
}
