use bytes::Bytes;

use crate::{
    nal::{is_first_slice, NalInfo},
    sei::parse_lkts,
    Codec,
};

/// One coded picture with the NAL units that belong to it (AUDs removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub nals: Vec<Bytes>,
    /// Contains an IDR (H.264) or IRAP (H.265) picture.
    pub key: bool,
    /// Capture time from an LKTS SEI, Unix microseconds.
    pub capture_us: Option<u64>,
    pub frame_id: Option<u32>,
}

/// Groups NAL units into access units.
///
/// An access unit ends when:
/// - an AUD arrives after a VCL NAL (an encoder that terminates every AU
///   with an AUD, or the leading AUD of the next AU);
/// - a NAL that may only start an AU (SPS, PPS, VPS, prefix SEI, ...) or the
///   first slice of a new picture arrives after a VCL NAL;
/// - with `complete_on_vcl`, as soon as a VCL NAL arrives. Only correct for
///   single-slice encoders (hardware encoders always are); it saves one frame
///   interval for streams with no AU terminator.
pub struct AuAssembler {
    codec: Codec,
    complete_on_vcl: bool,
    nals: Vec<Bytes>,
    has_vcl: bool,
}

impl AuAssembler {
    pub fn new(codec: Codec, complete_on_vcl: bool) -> Self {
        Self {
            codec,
            complete_on_vcl,
            nals: Vec::new(),
            has_vcl: false,
        }
    }

    pub fn reset(&mut self) {
        self.nals.clear();
        self.has_vcl = false;
    }

    pub fn push(&mut self, nal: Bytes, out: &mut Vec<AccessUnit>) {
        let Some(info) = NalInfo::parse(self.codec, &nal) else {
            return;
        };
        if info.is_aud() {
            if self.has_vcl {
                self.emit(out);
            }
            return;
        }
        if self.has_vcl && (info.starts_access_unit() || (info.is_vcl() && is_first_slice(self.codec, &nal))) {
            self.emit(out);
        }
        self.nals.push(nal);
        if info.is_vcl() {
            self.has_vcl = true;
            if self.complete_on_vcl {
                self.emit(out);
            }
        }
    }

    fn emit(&mut self, out: &mut Vec<AccessUnit>) {
        let nals = std::mem::take(&mut self.nals);
        self.has_vcl = false;
        let mut au = AccessUnit {
            nals,
            key: false,
            capture_us: None,
            frame_id: None,
        };
        for nal in &au.nals {
            let Some(info) = NalInfo::parse(self.codec, nal) else {
                continue;
            };
            if info.is_keyframe() {
                au.key = true;
            } else if info.is_prefix_sei() && au.capture_us.is_none() {
                if let Some(ts) = parse_lkts(self.codec, nal) {
                    au.capture_us = ts.timestamp_us;
                    au.frame_id = ts.frame_id;
                }
            }
        }
        out.push(au);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sei::tests::lkts_sei;

    fn b(v: &[u8]) -> Bytes {
        Bytes::copy_from_slice(v)
    }

    const SPS: &[u8] = &[0x67, 0x42];
    const PPS: &[u8] = &[0x68, 0xce];
    const IDR: &[u8] = &[0x65, 0x88];
    const P: &[u8] = &[0x41, 0x9a];
    const P_SLICE2: &[u8] = &[0x41, 0x40];
    const AUD: &[u8] = &[0x09, 0xf0];

    fn feed(a: &mut AuAssembler, nals: &[&[u8]]) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        for n in nals {
            a.push(b(n), &mut out);
        }
        out
    }

    #[test]
    fn trailing_aud_completes_immediately() {
        let mut a = AuAssembler::new(Codec::H264, false);
        let sei = lkts_sei(Codec::H264, 7, 1);
        let out = feed(&mut a, &[&sei, SPS, PPS, IDR, AUD]);
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
        assert_eq!(out[0].capture_us, Some(7));
        assert_eq!(out[0].nals.len(), 4);
        let out = feed(&mut a, &[P, AUD]);
        assert_eq!(out.len(), 1);
        assert!(!out[0].key);
    }

    #[test]
    fn without_terminator_next_au_closes_previous() {
        let mut a = AuAssembler::new(Codec::H264, false);
        assert!(feed(&mut a, &[SPS, PPS, IDR]).is_empty());
        let out = feed(&mut a, &[P]);
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
        // Leading AUD of the next AU closes the P frame.
        let out = feed(&mut a, &[AUD]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].nals, vec![b(P)]);
    }

    #[test]
    fn multi_slice_pictures_stay_together() {
        let mut a = AuAssembler::new(Codec::H264, false);
        let out = feed(&mut a, &[P, P_SLICE2, AUD]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].nals.len(), 2);
    }

    #[test]
    fn complete_on_vcl() {
        let mut a = AuAssembler::new(Codec::H264, true);
        let out = feed(&mut a, &[SPS, PPS, IDR]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].nals.len(), 3);
        // A trailing AUD after an emitted AU is dropped silently.
        assert!(feed(&mut a, &[AUD]).is_empty());
    }

    #[test]
    fn h265_access_units() {
        let vps: &[u8] = &[0x40, 0x01];
        let sps: &[u8] = &[0x42, 0x01];
        let pps: &[u8] = &[0x44, 0x01];
        let idr: &[u8] = &[0x26, 0x01, 0xaf];
        let trail: &[u8] = &[0x02, 0x01, 0xd0];
        let suffix_sei: &[u8] = &[0x50, 0x01, 0x05];
        let aud: &[u8] = &[0x46, 0x01, 0x50];
        let sei = lkts_sei(Codec::H265, 99, 3);
        let mut a = AuAssembler::new(Codec::H265, false);
        let out = feed(&mut a, &[&sei, vps, sps, pps, idr, suffix_sei, aud, trail]);
        assert_eq!(out.len(), 1);
        assert!(out[0].key);
        assert_eq!(out[0].capture_us, Some(99));
        assert_eq!(out[0].frame_id, Some(3));
        assert_eq!(out[0].nals.len(), 6);
        let out = feed(&mut a, &[aud]);
        assert!(!out[0].key);
    }
}
