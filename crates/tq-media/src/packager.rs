use bytes::{BufMut, Bytes, BytesMut};

use crate::{codec_string::codec_string, nal::NalInfo, AccessUnit, Codec};

/// An access unit ready to send: Annex B bytes, parameter sets guaranteed
/// in front of every keyframe.
#[derive(Debug, Clone)]
pub struct Frame {
    pub data: Bytes,
    pub key: bool,
    /// WebCodecs codec string derived from the current SPS.
    pub codec_string: String,
    pub capture_us: Option<u64>,
    pub frame_id: Option<u32>,
}

/// Turns access units into [`Frame`]s.
///
/// Drops everything before the first keyframe, remembers the latest
/// VPS/SPS/PPS and puts them in front of any keyframe that arrives without
/// them, so a receiver can always start decoding at a keyframe.
pub struct Packager {
    codec: Codec,
    vps: Option<Bytes>,
    sps: Option<Bytes>,
    pps: Option<Bytes>,
    codec_string: Option<String>,
    started: bool,
}

impl Packager {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            vps: None,
            sps: None,
            pps: None,
            codec_string: None,
            started: false,
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Restart from the next keyframe (after a source reconnect).
    pub fn reset(&mut self) {
        self.started = false;
    }

    pub fn package(&mut self, au: AccessUnit) -> Option<Frame> {
        let (mut has_vps, mut has_sps, mut has_pps) = (false, false, false);
        for nal in &au.nals {
            let Some(info) = NalInfo::parse(self.codec, nal) else {
                continue;
            };
            if info.is_vps() {
                has_vps = true;
                self.vps = Some(nal.clone());
            } else if info.is_sps() {
                has_sps = true;
                if self.sps.as_ref() != Some(nal) {
                    self.codec_string = codec_string(self.codec, nal);
                    self.sps = Some(nal.clone());
                }
            } else if info.is_pps() {
                has_pps = true;
                self.pps = Some(nal.clone());
            }
        }

        if !self.started {
            if !au.key || self.sps.is_none() || self.pps.is_none() || self.codec_string.is_none() {
                return None;
            }
            if self.codec == Codec::H265 && self.vps.is_none() {
                return None;
            }
            self.started = true;
        }

        let mut prefix: Vec<&Bytes> = Vec::new();
        if au.key {
            if self.codec == Codec::H265 && !has_vps {
                prefix.extend(self.vps.as_ref());
            }
            if !has_sps {
                prefix.extend(self.sps.as_ref());
            }
            if !has_pps {
                prefix.extend(self.pps.as_ref());
            }
        }
        let size: usize =
            prefix.iter().map(|n| n.len() + 4).sum::<usize>() + au.nals.iter().map(|n| n.len() + 4).sum::<usize>();
        let mut data = BytesMut::with_capacity(size);
        for nal in prefix.into_iter().chain(au.nals.iter()) {
            data.put_slice(&[0, 0, 0, 1]);
            data.put_slice(nal);
        }

        Some(Frame {
            data: data.freeze(),
            key: au.key,
            codec_string: self.codec_string.clone()?,
            capture_us: au.capture_us,
            frame_id: au.frame_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(nals: &[&[u8]], key: bool) -> AccessUnit {
        AccessUnit {
            nals: nals.iter().map(|n| Bytes::copy_from_slice(n)).collect(),
            key,
            capture_us: None,
            frame_id: None,
        }
    }

    const SPS: &[u8] = &[0x67, 0x42, 0xc0, 0x1f];
    const PPS: &[u8] = &[0x68, 0xce];
    const IDR: &[u8] = &[0x65, 0x88];
    const P: &[u8] = &[0x41, 0x9a];

    #[test]
    fn waits_for_keyframe_and_repeats_parameter_sets() {
        let mut p = Packager::new(Codec::H264);
        assert!(p.package(au(&[P], false)).is_none());
        let f = p.package(au(&[SPS, PPS, IDR], true)).unwrap();
        assert!(f.key);
        assert_eq!(f.codec_string, "avc1.42c01f");
        assert_eq!(
            &f.data[..],
            &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88]
        );
        let f = p.package(au(&[P], false)).unwrap();
        assert_eq!(&f.data[..], &[0, 0, 0, 1, 0x41, 0x9a]);
        // A bare IDR gets the cached SPS/PPS.
        let f = p.package(au(&[IDR], true)).unwrap();
        assert_eq!(f.data.len(), 20);
    }
}
