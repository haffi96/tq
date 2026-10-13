use crate::{
    bits::{unescape_rbsp, BitReader},
    nal::NalInfo,
    Codec,
};

/// WebCodecs / RFC 6381 codec string for an SPS NAL unit.
pub fn codec_string(codec: Codec, sps: &[u8]) -> Option<String> {
    match codec {
        Codec::H264 => h264_codec_string(sps),
        Codec::H265 => h265_codec_string(sps),
    }
}

/// `avc1.PPCCLL`: profile_idc, constraint flags and level_idc straight from
/// the SPS.
pub fn h264_codec_string(sps: &[u8]) -> Option<String> {
    if !NalInfo::parse(Codec::H264, sps)?.is_sps() || sps.len() < 4 {
        return None;
    }
    Some(format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// `hev1.<space><profile>.<compat>.<tier><level>.<constraints>` from the
/// SPS's profile_tier_level (ISO/IEC 14496-15 Annex E). `hev1` because
/// parameter sets travel in band.
pub fn h265_codec_string(sps: &[u8]) -> Option<String> {
    if !NalInfo::parse(Codec::H265, sps)?.is_sps() {
        return None;
    }
    let rbsp = unescape_rbsp(&sps[2..]);
    let mut r = BitReader::new(&rbsp);
    r.bits(4)?; // sps_video_parameter_set_id
    r.bits(3)?; // sps_max_sub_layers_minus1
    r.bits(1)?; // sps_temporal_id_nesting_flag
    let profile_space = r.bits(2)?;
    let tier = r.bits(1)?;
    let profile_idc = r.bits(5)?;
    let compat = r.bits(32)?;
    let mut constraints = [0u8; 6];
    for c in &mut constraints {
        *c = r.bits(8)? as u8;
    }
    let level_idc = r.bits(8)?;

    let mut s = String::from("hev1.");
    if profile_space > 0 {
        s.push((b'A' + profile_space as u8 - 1) as char);
    }
    s.push_str(&format!("{profile_idc}.{:X}.", compat.reverse_bits()));
    s.push(if tier == 1 { 'H' } else { 'L' });
    s.push_str(&level_idc.to_string());
    let used = constraints.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    for c in &constraints[..used] {
        s.push_str(&format!(".{c:X}"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264() {
        assert_eq!(
            h264_codec_string(&[0x67, 0x42, 0xc0, 0x1f, 0xda]).as_deref(),
            Some("avc1.42c01f")
        );
        assert_eq!(h264_codec_string(&[0x68, 0x42, 0xc0, 0x1f]), None);
    }

    #[test]
    fn h265_main_level_3_1() {
        // SPS of a 1280x720 Main profile stream (x265 / vtenc shape):
        // vps_id 0, max_sub_layers_minus1 0, nesting 1, Main, compat 0x60000000,
        // progressive + non-packed + frame-only (0xb0), level 93.
        let sps = [
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5d,
            0xa0,
        ];
        assert_eq!(h265_codec_string(&sps).as_deref(), Some("hev1.1.6.L93.B0"));
    }
}
