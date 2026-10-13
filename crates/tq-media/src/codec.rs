use std::{fmt, str::FromStr};

/// Video codec of an elementary stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    /// Length of the NAL unit header in bytes.
    pub fn header_len(self) -> usize {
        match self {
            Codec::H264 => 1,
            Codec::H265 => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        }
    }

    /// Guesses the codec from the first NAL unit of a stream. Encoders start a
    /// stream with an AUD, SEI or parameter sets, whose headers differ between
    /// the two codecs: an H.265 header is two bytes with `nuh_layer_id` 0 and
    /// `nuh_temporal_id_plus1` 1 (second byte 0x01), and its first byte read
    /// as H.264 would be an SEI/AUD with a non-zero `nal_ref_idc` or a
    /// reserved type, neither of which an H.264 encoder emits.
    pub fn detect(nal: &[u8]) -> Option<Codec> {
        let b0 = *nal.first()?;
        if b0 & 0x80 != 0 {
            return None;
        }
        if nal.len() >= 2 && nal[1] == 0x01 {
            let t = (b0 >> 1) & 0x3f;
            if matches!(t, 32..=35 | 39 | 16..=21) {
                return Some(Codec::H265);
            }
        }
        match b0 & 0x1f {
            1 | 5 | 6 | 7 | 8 | 9 => Some(Codec::H264),
            _ => None,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Codec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Ok(Codec::H264),
            "h265" | "hevc" => Ok(Codec::H265),
            other => Err(format!("unknown codec '{other}' (use h264 or h265)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_codec_from_first_nal() {
        assert_eq!(Codec::detect(&[0x67, 0x42]), Some(Codec::H264)); // SPS
        assert_eq!(Codec::detect(&[0x09, 0xf0]), Some(Codec::H264)); // AUD
        assert_eq!(Codec::detect(&[0x06, 0x05]), Some(Codec::H264)); // SEI
        assert_eq!(Codec::detect(&[0x40, 0x01, 0x0c]), Some(Codec::H265)); // VPS
        assert_eq!(Codec::detect(&[0x46, 0x01, 0x50]), Some(Codec::H265)); // AUD
        assert_eq!(Codec::detect(&[0x4e, 0x01, 0x05]), Some(Codec::H265)); // prefix SEI
        assert_eq!(Codec::detect(&[0x80]), None);
    }
}
