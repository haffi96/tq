use bytes::Bytes;

use crate::{nal::NalInfo, Codec};

/// How NAL units are delimited in the incoming byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Annex B: `00 00 01` / `00 00 00 01` start codes.
    AnnexB,
    /// Every NAL is preceded by its length as a 4-byte big-endian integer
    /// (GStreamer `stream-format=avc` / `hvc1` / `hev1`).
    LengthPrefixed,
}

/// Configured framing; `Auto` decides from the first four bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingMode {
    Auto,
    Fixed(Framing),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FramerError {
    #[error("invalid NAL length {0} (stream is not 4-byte length-prefixed, or framing was lost)")]
    BadLength(usize),
    #[error("NAL unit larger than {0} bytes without a start code")]
    TooLarge(usize),
    #[error("could not detect the codec from the first NAL unit (type byte {0:#04x}); set it explicitly")]
    UnknownCodec(u8),
}

/// Splits a TCP byte stream into NAL units, however the bytes were chunked
/// in transit.
///
/// Latency: a length-prefixed NAL is complete as soon as its last byte
/// arrives. An Annex B NAL is only known to be complete when the next start
/// code arrives, except for an access unit delimiter, which has a fixed size
/// and is emitted as soon as it is in. Encoders that terminate every access
/// unit with an AUD (gstreamer-source `--au-terminator`) therefore deliver
/// each frame immediately instead of one frame interval later.
pub struct NalFramer {
    mode: FramingMode,
    framing: Option<Framing>,
    codec: Option<Codec>,
    buf: Vec<u8>,
    /// Start of the current NAL's payload in `buf` (Annex B only).
    nal_start: Option<usize>,
    /// Where the next start-code search resumes (Annex B only).
    scan: usize,
    max_nal: usize,
}

pub const DEFAULT_MAX_NAL: usize = 8 << 20;

impl NalFramer {
    pub fn new(mode: FramingMode, codec: Option<Codec>) -> Self {
        Self {
            mode,
            framing: match mode {
                FramingMode::Auto => None,
                FramingMode::Fixed(f) => Some(f),
            },
            codec,
            buf: Vec::with_capacity(256 << 10),
            nal_start: None,
            scan: 0,
            max_nal: DEFAULT_MAX_NAL,
        }
    }

    /// Detected or configured framing, once known.
    pub fn framing(&self) -> Option<Framing> {
        self.framing
    }

    /// Detected or configured codec, once known.
    pub fn codec(&self) -> Option<Codec> {
        self.codec
    }

    /// Forgets buffered bytes (used after a reconnect). Auto-detected
    /// framing is detected again.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.nal_start = None;
        self.scan = 0;
        if self.mode == FramingMode::Auto {
            self.framing = None;
        }
    }

    /// Appends a chunk and pushes every NAL unit it completes onto `out`.
    /// On error the framer is reset; the caller should reconnect.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<Bytes>) -> Result<(), FramerError> {
        self.buf.extend_from_slice(data);
        if self.framing.is_none() {
            if self.buf.len() < 4 {
                return Ok(());
            }
            let b = &self.buf;
            let annexb = b[..3] == [0, 0, 1] || b[..4] == [0, 0, 0, 1];
            self.framing = Some(if annexb {
                Framing::AnnexB
            } else {
                Framing::LengthPrefixed
            });
        }
        let result = match self.framing {
            Some(Framing::AnnexB) => self.push_annexb(out),
            Some(Framing::LengthPrefixed) => self.push_length_prefixed(out),
            None => unreachable!(),
        };
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn detect_codec(codec: &mut Option<Codec>, nal: &[u8]) -> Result<(), FramerError> {
        if codec.is_none() {
            *codec = Some(Codec::detect(nal).ok_or(FramerError::UnknownCodec(nal[0]))?);
        }
        Ok(())
    }

    fn push_length_prefixed(&mut self, out: &mut Vec<Bytes>) -> Result<(), FramerError> {
        let mut off = 0;
        while self.buf.len() - off >= 4 {
            let p = &self.buf[off..];
            let len = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) as usize;
            if len == 0 || len > self.max_nal {
                return Err(FramerError::BadLength(len));
            }
            if p.len() < 4 + len {
                break;
            }
            let nal = &p[4..4 + len];
            Self::detect_codec(&mut self.codec, nal)?;
            out.push(Bytes::copy_from_slice(nal));
            off += 4 + len;
        }
        self.buf.drain(..off);
        Ok(())
    }

    fn push_annexb(&mut self, out: &mut Vec<Bytes>) -> Result<(), FramerError> {
        let mut consumed = 0;
        loop {
            let Some(start) = self.nal_start else {
                // Discard everything before the first start code.
                match find_start_code(&self.buf, self.scan.max(consumed)) {
                    Some(i) => {
                        self.nal_start = Some(i + 3);
                        self.scan = i + 3;
                        consumed = i + 3;
                        continue;
                    }
                    None => {
                        // Keep two bytes: they may begin a start code.
                        consumed = self.buf.len().saturating_sub(2).max(consumed);
                        self.scan = consumed;
                        break;
                    }
                }
            };

            // An AUD has a fixed size, so it is complete without the next
            // start code.
            if self.buf.len() >= start + 3 {
                let head = &self.buf[start..];
                Self::detect_codec(&mut self.codec, head)?;
            }
            if let Some(codec) = self.codec {
                let aud_len = NalInfo::aud_len(codec);
                if self.buf.len() >= start + aud_len {
                    let nal = &self.buf[start..start + aud_len];
                    if NalInfo::parse(codec, nal).is_some_and(|n| n.is_aud()) {
                        out.push(Bytes::copy_from_slice(nal));
                        self.nal_start = None;
                        self.scan = start + aud_len;
                        consumed = start + aud_len;
                        continue;
                    }
                }
            }

            match find_start_code(&self.buf, self.scan.max(start)) {
                Some(next) => {
                    let mut end = next;
                    // `00 00 00 01`: the leading zero belongs to the start code
                    // (trailing_zero_8bits), not to the NAL.
                    while end > start && self.buf[end - 1] == 0 {
                        end -= 1;
                    }
                    if end > start {
                        let nal = &self.buf[start..end];
                        Self::detect_codec(&mut self.codec, nal)?;
                        out.push(Bytes::copy_from_slice(nal));
                    }
                    self.nal_start = Some(next + 3);
                    self.scan = next + 3;
                    consumed = next + 3;
                }
                None => {
                    if self.buf.len() - start > self.max_nal {
                        return Err(FramerError::TooLarge(self.max_nal));
                    }
                    self.scan = self.buf.len().saturating_sub(2).max(start);
                    consumed = start;
                    break;
                }
            }
        }

        if consumed > 0 {
            self.buf.drain(..consumed);
            self.scan -= consumed.min(self.scan);
            if let Some(s) = self.nal_start.as_mut() {
                *s -= consumed;
            }
        }
        Ok(())
    }
}

/// Index of the next `00 00 01` at or after `from`.
fn find_start_code(buf: &[u8], from: usize) -> Option<usize> {
    if buf.len() < 3 {
        return None;
    }
    let mut i = from;
    while i + 2 < buf.len() {
        // Skip ahead quickly: a start code needs buf[i + 2] == 1 and
        // buf[i + 1] == 0.
        let b2 = buf[i + 2];
        if b2 > 1 {
            i += 3;
            continue;
        }
        if b2 == 1 && buf[i + 1] == 0 && buf[i] == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    fn run(framer: &mut NalFramer, stream: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for c in stream.chunks(chunk) {
            framer.push(c, &mut out).unwrap();
        }
        out.into_iter().map(|b| b.to_vec()).collect()
    }

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f, 0x00, 0x00, 0x03, 0x00, 0x01];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x01, 0x10];
    const AUD: &[u8] = &[0x09, 0xf0];

    #[test]
    fn annexb_any_chunking() {
        let stream = annexb(&[SPS, PPS, IDR, AUD]);
        for chunk in [1, 2, 3, 5, 7, 64] {
            let mut f = NalFramer::new(FramingMode::Auto, None);
            let nals = run(&mut f, &stream, chunk);
            // SPS and PPS are completed by the next start code; IDR by the
            // AUD's start code; the AUD by its fixed size.
            assert_eq!(
                nals,
                vec![SPS.to_vec(), PPS.to_vec(), IDR.to_vec(), AUD.to_vec()],
                "chunk {chunk}"
            );
            assert_eq!(f.framing(), Some(Framing::AnnexB));
            assert_eq!(f.codec(), Some(Codec::H264));
        }
    }

    #[test]
    fn annexb_last_nal_waits_for_next_start_code() {
        let mut f = NalFramer::new(FramingMode::Auto, None);
        let mut out = Vec::new();
        f.push(&annexb(&[SPS, IDR]), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        f.push(&[0, 0, 1, 0x09], &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].as_ref(), IDR);
    }

    #[test]
    fn annexb_three_byte_start_codes_and_garbage() {
        let mut stream = vec![0xff, 0x12];
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(SPS);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(PPS);
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(AUD);
        let mut f = NalFramer::new(FramingMode::Fixed(Framing::AnnexB), Some(Codec::H264));
        let nals = run(&mut f, &stream, 4);
        assert_eq!(nals, vec![SPS.to_vec(), PPS.to_vec(), AUD.to_vec()]);
    }

    #[test]
    fn h265_aud_completes_immediately() {
        let vps: &[u8] = &[0x40, 0x01, 0x0c, 0x01];
        let idr: &[u8] = &[0x26, 0x01, 0xaf, 0x01];
        let aud: &[u8] = &[0x46, 0x01, 0x50];
        let mut f = NalFramer::new(FramingMode::Auto, None);
        let nals = run(&mut f, &annexb(&[vps, idr, aud]), 3);
        assert_eq!(nals, vec![vps.to_vec(), idr.to_vec(), aud.to_vec()]);
        assert_eq!(f.codec(), Some(Codec::H265));
    }

    #[test]
    fn length_prefixed_any_chunking() {
        let stream = length_prefixed(&[SPS, PPS, IDR, AUD]);
        for chunk in [1, 3, 4, 9, 100] {
            let mut f = NalFramer::new(FramingMode::Auto, None);
            let nals = run(&mut f, &stream, chunk);
            assert_eq!(nals, vec![SPS.to_vec(), PPS.to_vec(), IDR.to_vec(), AUD.to_vec()]);
            assert_eq!(f.framing(), Some(Framing::LengthPrefixed));
        }
    }

    #[test]
    fn length_prefixed_rejects_bad_length() {
        let mut f = NalFramer::new(FramingMode::Fixed(Framing::LengthPrefixed), Some(Codec::H264));
        let mut out = Vec::new();
        assert_eq!(
            f.push(&[0xff, 0xff, 0xff, 0xff], &mut out),
            Err(FramerError::BadLength(0xffff_ffff))
        );
    }
}
