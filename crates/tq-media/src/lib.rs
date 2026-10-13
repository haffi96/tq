//! H.264 / H.265 elementary stream handling for the publisher.
//!
//! The camera encoder serves a pre-encoded stream over TCP, either Annex B
//! (start codes) or 4-byte length-prefixed. This crate turns those bytes into
//! complete access units without decoding anything:
//!
//! ```text
//! TCP bytes -> NalFramer -> NAL units -> AuAssembler -> AccessUnit -> Packager -> Frame
//! ```
//!
//! The [`Packager`] output is always Annex B with the parameter sets in front of
//! every keyframe, which is what WebCodecs' `VideoDecoder` expects when it is
//! configured without a `description`.

mod au;
mod bits;
mod codec;
mod codec_string;
mod framer;
mod nal;
mod packager;
mod sei;

pub use au::{AccessUnit, AuAssembler};
pub use bits::{unescape_rbsp, BitReader};
pub use codec::Codec;
pub use codec_string::{codec_string, h264_codec_string, h265_codec_string};
pub use framer::{FramerError, Framing, FramingMode, NalFramer};
pub use nal::NalInfo;
pub use packager::{Frame, Packager};
pub use sei::{parse_lkts, SeiTimestamp, SEI_UUID};
