use crate::Codec;

/// Classification of one NAL unit (without start code or length prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalInfo {
    pub codec: Codec,
    pub nal_type: u8,
}

impl NalInfo {
    pub fn parse(codec: Codec, nal: &[u8]) -> Option<Self> {
        let b0 = *nal.first()?;
        let nal_type = match codec {
            Codec::H264 => b0 & 0x1f,
            Codec::H265 => {
                if nal.len() < 2 {
                    return None;
                }
                (b0 >> 1) & 0x3f
            }
        };
        Some(Self { codec, nal_type })
    }

    /// Slice data (a coded picture or part of one).
    pub fn is_vcl(&self) -> bool {
        match self.codec {
            Codec::H264 => matches!(self.nal_type, 1..=5),
            Codec::H265 => self.nal_type <= 31,
        }
    }

    /// A picture a decoder can start from: IDR for H.264, IRAP (BLA, IDR,
    /// CRA) for H.265.
    pub fn is_keyframe(&self) -> bool {
        match self.codec {
            Codec::H264 => self.nal_type == 5,
            Codec::H265 => matches!(self.nal_type, 16..=21),
        }
    }

    pub fn is_aud(&self) -> bool {
        match self.codec {
            Codec::H264 => self.nal_type == 9,
            Codec::H265 => self.nal_type == 35,
        }
    }

    /// Fixed size of an access unit delimiter NAL (header + one payload byte).
    pub fn aud_len(codec: Codec) -> usize {
        codec.header_len() + 1
    }

    pub fn is_prefix_sei(&self) -> bool {
        match self.codec {
            Codec::H264 => self.nal_type == 6,
            Codec::H265 => self.nal_type == 39,
        }
    }

    pub fn is_vps(&self) -> bool {
        self.codec == Codec::H265 && self.nal_type == 32
    }

    pub fn is_sps(&self) -> bool {
        match self.codec {
            Codec::H264 => self.nal_type == 7,
            Codec::H265 => self.nal_type == 33,
        }
    }

    pub fn is_pps(&self) -> bool {
        match self.codec {
            Codec::H264 => self.nal_type == 8,
            Codec::H265 => self.nal_type == 34,
        }
    }

    /// Non-VCL NAL types that may only appear before the first VCL NAL of an
    /// access unit, so seeing one after a VCL NAL means a new access unit has
    /// started (H.264 7.4.1.2.3, H.265 7.4.2.4.4).
    pub fn starts_access_unit(&self) -> bool {
        match self.codec {
            Codec::H264 => matches!(self.nal_type, 6..=9 | 14..=18),
            Codec::H265 => matches!(self.nal_type, 32..=35 | 39 | 41..=44 | 48..=55),
        }
    }
}

/// Whether a VCL NAL is the first slice of its picture: `first_mb_in_slice`
/// is 0 in H.264 (an Exp-Golomb 0 is the single bit `1`), and
/// `first_slice_segment_in_pic_flag` is set in H.265. Both are the first bit
/// after the NAL header.
pub fn is_first_slice(codec: Codec, nal: &[u8]) -> bool {
    nal.get(codec.header_len()).map(|b| b & 0x80 != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_h264() {
        let idr = NalInfo::parse(Codec::H264, &[0x65, 0x88]).unwrap();
        assert!(idr.is_vcl() && idr.is_keyframe());
        let sps = NalInfo::parse(Codec::H264, &[0x67]).unwrap();
        assert!(sps.is_sps() && sps.starts_access_unit() && !sps.is_vcl());
        let aud = NalInfo::parse(Codec::H264, &[0x09, 0xf0]).unwrap();
        assert!(aud.is_aud());
    }

    #[test]
    fn classifies_h265() {
        let idr = NalInfo::parse(Codec::H265, &[0x26, 0x01, 0xaf]).unwrap(); // IDR_W_RADL
        assert_eq!(idr.nal_type, 19);
        assert!(idr.is_vcl() && idr.is_keyframe());
        let trail = NalInfo::parse(Codec::H265, &[0x02, 0x01, 0xd0]).unwrap(); // TRAIL_R
        assert!(trail.is_vcl() && !trail.is_keyframe());
        let vps = NalInfo::parse(Codec::H265, &[0x40, 0x01]).unwrap();
        assert!(vps.is_vps() && vps.starts_access_unit());
        let suffix_sei = NalInfo::parse(Codec::H265, &[0x50, 0x01]).unwrap();
        assert_eq!(suffix_sei.nal_type, 40);
        assert!(!suffix_sei.starts_access_unit());
    }

    #[test]
    fn first_slice_flag() {
        assert!(is_first_slice(Codec::H264, &[0x65, 0x88]));
        assert!(!is_first_slice(Codec::H264, &[0x65, 0x48]));
        assert!(is_first_slice(Codec::H265, &[0x26, 0x01, 0xaf]));
        assert!(!is_first_slice(Codec::H265, &[0x02, 0x01, 0x50]));
    }
}
