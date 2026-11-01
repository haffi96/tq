//! Video tracks from an encoder's output: H.264 or H.265 bytes in, frames
//! on the track out, in groups that start at keyframes.

use std::sync::Arc;

use bytes::Bytes;
use tq_media::{AccessUnit, AuAssembler, Codec, FramerError, FramingMode, NalFramer, Packager};
use tq_transport::track::Track;
use tq_wire::{now_us, FrameHeader, MediaFrame, FLAG_CAPTURE_FROM_SEI, FLAG_KEY};

use crate::Publication;

/// What a [`VideoWriter`] is given.
#[derive(Debug, Clone, Copy)]
pub struct VideoOptions {
    /// `None`: detected from the first NAL unit.
    pub codec: Option<Codec>,
    /// Annex B or length-prefixed; `Auto` decides from the first bytes.
    pub framing: FramingMode,
    /// Complete an access unit as soon as its slice arrives. Only for
    /// single-slice encoders that do not end access units with an AUD;
    /// saves a frame interval there. [`VideoWriter::write_frame`] needs no
    /// such help.
    pub complete_on_vcl: bool,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            codec: None,
            framing: FramingMode::Auto,
            complete_on_vcl: false,
        }
    }
}

/// Writes one video track. Frames before the first keyframe are dropped;
/// parameter sets go in front of every keyframe, so a subscriber can start
/// at any group. A capture time in an LKTS SEI is kept, so latency is
/// measured from the camera; otherwise from when the frame was written.
///
/// Get one with [`Publication::video`]. Writing is synchronous and cheap,
/// so it can run on the encoder's own thread.
pub struct VideoWriter {
    p: Arc<Publication>,
    id: String,
    track: Arc<Track>,
    opts: VideoOptions,
    framer: NalFramer,
    pipeline: Option<(AuAssembler, Packager)>,
    group: u64,
    index: u32,
    live: bool,
    codec_string: Option<String>,
    frames: u64,
    nals: Vec<Bytes>,
    aus: Vec<AccessUnit>,
}

impl VideoWriter {
    pub(crate) fn new(p: Arc<Publication>, id: &str, track: Arc<Track>, opts: VideoOptions) -> Self {
        Self {
            p,
            id: id.to_string(),
            track,
            framer: NalFramer::new(opts.framing, opts.codec),
            opts,
            pipeline: None,
            group: 0,
            index: 0,
            live: false,
            codec_string: None,
            frames: 0,
            nals: Vec::new(),
            aus: Vec::new(),
        }
    }

    /// Part of an elementary stream, chunked any way (a pipe, a socket). A
    /// frame goes out once the stream shows where it ends: an AUD, the
    /// next frame's first NAL unit, or with `complete_on_vcl` its slice.
    ///
    /// On an error the writer starts over, as after [`VideoWriter::reset`].
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), FramerError> {
        self.nals.clear();
        if let Err(e) = self.framer.push(bytes, &mut self.nals) {
            self.reset(&e.to_string());
            return Err(e);
        }
        self.assemble(false, None);
        Ok(())
    }

    /// Exactly one access unit (one encoded picture with its parameter
    /// sets and SEI, as most encoder APIs return it): it goes out now.
    pub fn write_frame(&mut self, au: &[u8]) -> Result<(), FramerError> {
        self.write_frame_inner(au, None)
    }

    /// [`VideoWriter::write_frame`], captured at `capture_us` (this
    /// machine's clock, Unix µs), for a camera that says when. Overrides
    /// an SEI capture time.
    pub fn write_frame_at(&mut self, au: &[u8], capture_us: u64) -> Result<(), FramerError> {
        self.write_frame_inner(au, Some(capture_us))
    }

    fn write_frame_inner(&mut self, au: &[u8], capture_us: Option<u64>) -> Result<(), FramerError> {
        self.nals.clear();
        let framed = self
            .framer
            .push(au, &mut self.nals)
            .and_then(|()| self.framer.flush(&mut self.nals));
        if let Err(e) = framed {
            self.reset(&e.to_string());
            return Err(e);
        }
        self.assemble(true, capture_us);
        Ok(())
    }

    /// Sends what [`VideoWriter::write`] still holds: the last frame of a
    /// stream that has ended, which nothing after it will complete.
    pub fn flush(&mut self) {
        self.nals.clear();
        if let Err(e) = self.framer.flush(&mut self.nals) {
            self.reset(&e.to_string());
            return;
        }
        self.assemble(true, None);
    }

    /// The source stopped: the track goes offline (subscribers hear why)
    /// and starts again at the next keyframe written.
    pub fn reset(&mut self, reason: &str) {
        self.framer.reset();
        self.pipeline = None;
        self.live = false;
        self.p.set_online(&self.id, false, reason);
        self.track.reset();
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Frames sent so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The codec, once known.
    pub fn codec(&self) -> Option<Codec> {
        self.framer.codec()
    }

    /// The WebCodecs codec string, once a keyframe's SPS has shown it.
    pub fn codec_string(&self) -> Option<&str> {
        self.codec_string.as_deref()
    }

    fn assemble(&mut self, flush: bool, capture_us: Option<u64>) {
        // A flush with no new NAL units still ends the access unit in
        // progress (length-prefixed NALs are out as soon as they arrive).
        if self.nals.is_empty() && !(flush && self.pipeline.is_some()) {
            return;
        }
        let (assembler, packager) = match self.pipeline.as_mut() {
            Some(p) => p,
            None => {
                let codec = self.framer.codec().expect("codec known once a NAL is out");
                tracing::info!(track = %self.id, %codec, framing = ?self.framer.framing(), "video stream detected");
                self.pipeline
                    .insert((AuAssembler::new(codec, self.opts.complete_on_vcl), Packager::new(codec)))
            }
        };
        self.aus.clear();
        for nal in self.nals.drain(..) {
            assembler.push(nal, &mut self.aus);
        }
        if flush {
            assembler.flush(&mut self.aus);
        }
        for au in self.aus.drain(..) {
            let Some(frame) = packager.package(au) else { continue };
            let publisher_us = now_us();
            if frame.key {
                self.group += 1;
                self.index = 0;
                if self.codec_string.as_deref() != Some(&frame.codec_string) {
                    tracing::info!(track = %self.id, codec = %frame.codec_string, "codec string");
                    self.codec_string = Some(frame.codec_string.clone());
                }
            }
            if !self.live {
                self.live = true;
                self.p.set_online(&self.id, true, "");
            }
            let mut flags = if frame.key { FLAG_KEY } else { 0 };
            // A real capture time sets the flag, whoever gave it.
            let capture_us = match capture_us.or(frame.capture_us) {
                Some(t) => {
                    flags |= FLAG_CAPTURE_FROM_SEI;
                    t
                }
                None => publisher_us,
            };
            self.frames += 1;
            self.track.push(MediaFrame {
                // The session stamps its own indices.
                broadcast: 0,
                track: 0,
                group: self.group,
                codec: frame.codec_string.as_str().into(),
                header: FrameHeader {
                    index: self.index,
                    flags,
                    capture_us,
                    publisher_us,
                    relay_us: 0,
                    frame_id: frame.frame_id.unwrap_or(0),
                },
                payload: frame.data,
            });
            self.index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f];
    const PPS: &[u8] = &[0x68, 0xce];
    const IDR: &[u8] = &[0x65, 0x88];

    #[test]
    fn flush_ends_the_last_length_prefixed_frame() {
        let (p, _inbox) = Publication::builder("rover").video("cam").build().unwrap();
        let mut w = p.video("cam").unwrap();
        let stream: Vec<u8> = [SPS, PPS, IDR]
            .iter()
            .flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], n].concat())
            .collect();
        w.write(&stream).unwrap();
        // Every NAL is out, but nothing yet says the picture has ended.
        assert_eq!(w.frames(), 0);
        w.flush();
        assert_eq!(w.frames(), 1);
        assert_eq!(p.video_track("cam").unwrap().subscribe().0.len(), 1);
    }
}
