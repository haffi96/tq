use crate::{bits::unescape_rbsp, nal::NalInfo, Codec};

/// UUID of the `user_data_unregistered` SEI that gstreamer-source puts in
/// front of every access unit.
pub const SEI_UUID: [u8; 16] = [
    0x3f, 0xa8, 0x5f, 0x64, 0x57, 0x17, 0x45, 0x62, 0xb3, 0xfc, 0x2c, 0x96, 0x3f, 0x66, 0xaf, 0xa6,
];
const MAGIC: &[u8; 4] = b"LKTS";
const TAG_USER_TIMESTAMP: u8 = 0x01;
const TAG_FRAME_ID: u8 = 0x02;

/// Capture metadata carried in an LKTS packet trailer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeiTimestamp {
    /// Wall-clock capture time, Unix microseconds (source machine clock).
    pub timestamp_us: Option<u64>,
    pub frame_id: Option<u32>,
}

/// Parses the LKTS timestamp / frame id out of a prefix SEI NAL, if it
/// carries one.
///
/// Payload: `UUID(16) | TLV... | trailer_len | "LKTS"`, where every TLV byte
/// and `trailer_len` are XORed with 0xFF. The RBSP is unescaped first; gstreamer-source does not insert
/// emulation prevention bytes, so the raw payload is tried as a fallback.
pub fn parse_lkts(codec: Codec, nal: &[u8]) -> Option<SeiTimestamp> {
    if !NalInfo::parse(codec, nal)?.is_prefix_sei() {
        return None;
    }
    let body = &nal[codec.header_len()..];
    parse_messages(&unescape_rbsp(body)).or_else(|| parse_messages(body))
}

fn parse_messages(rbsp: &[u8]) -> Option<SeiTimestamp> {
    let mut p = 0;
    // Stop at rbsp_trailing_bits (0x80).
    while p + 2 <= rbsp.len() && rbsp[p] != 0x80 {
        let mut payload_type = 0usize;
        while *rbsp.get(p)? == 0xff {
            payload_type += 255;
            p += 1;
        }
        payload_type += rbsp[p] as usize;
        p += 1;
        let mut size = 0usize;
        while *rbsp.get(p)? == 0xff {
            size += 255;
            p += 1;
        }
        size += *rbsp.get(p)? as usize;
        p += 1;
        let payload = rbsp.get(p..p + size)?;
        p += size;
        if payload_type == 5 && payload.len() >= 16 && payload[..16] == SEI_UUID {
            if let Some(ts) = parse_trailer(&payload[16..]) {
                return Some(ts);
            }
        }
    }
    None
}

fn parse_trailer(data: &[u8]) -> Option<SeiTimestamp> {
    let n = data.len();
    if n < 5 || &data[n - 4..] != MAGIC {
        return None;
    }
    let trailer_len = (data[n - 5] ^ 0xff) as usize;
    if trailer_len < 5 || trailer_len > n {
        return None;
    }
    let mut tlv = &data[n - trailer_len..n - 5];
    let mut out = SeiTimestamp::default();
    while tlv.len() >= 2 {
        let tag = tlv[0] ^ 0xff;
        let len = (tlv[1] ^ 0xff) as usize;
        let value: Vec<u8> = tlv.get(2..2 + len)?.iter().map(|b| b ^ 0xff).collect();
        match (tag, len) {
            (TAG_USER_TIMESTAMP, 8) => out.timestamp_us = Some(u64::from_be_bytes(value.try_into().ok()?)),
            (TAG_FRAME_ID, 4) => out.frame_id = Some(u32::from_be_bytes(value.try_into().ok()?)),
            _ => {}
        }
        tlv = &tlv[2 + len..];
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds an SEI NAL the way gstreamer-source's sei.py does.
    pub fn lkts_sei(codec: Codec, ts: u64, frame_id: u32) -> Vec<u8> {
        let mut trailer = vec![TAG_USER_TIMESTAMP ^ 0xff, 8 ^ 0xff];
        trailer.extend(ts.to_be_bytes().iter().map(|b| b ^ 0xff));
        trailer.extend([TAG_FRAME_ID ^ 0xff, 4 ^ 0xff]);
        trailer.extend(frame_id.to_be_bytes().iter().map(|b| b ^ 0xff));
        let len = trailer.len() + 1 + 4;
        trailer.push(len as u8 ^ 0xff);
        trailer.extend_from_slice(MAGIC);
        let mut payload = SEI_UUID.to_vec();
        payload.extend(trailer);
        let mut nal = match codec {
            Codec::H264 => vec![0x06],
            Codec::H265 => vec![0x4e, 0x01],
        };
        nal.push(5);
        nal.push(payload.len() as u8);
        nal.extend(payload);
        nal.push(0x80);
        nal
    }

    #[test]
    fn parses_h264_and_h265_lkts() {
        for codec in [Codec::H264, Codec::H265] {
            let nal = lkts_sei(codec, 1_759_000_000_123_456, 42);
            let ts = parse_lkts(codec, &nal).unwrap();
            assert_eq!(ts.timestamp_us, Some(1_759_000_000_123_456));
            assert_eq!(ts.frame_id, Some(42));
        }
    }

    #[test]
    fn parses_the_streaming_tools_sample() {
        // First SEI of streaming-tools samples/generated_sei.h264 shape:
        // a timestamp whose bytes contain 0xff, so the XORed form has zeros.
        let nal = lkts_sei(Codec::H264, 0x0000_ff00_ff00_0001, 1);
        assert_eq!(
            parse_lkts(Codec::H264, &nal).unwrap().timestamp_us,
            Some(0x0000_ff00_ff00_0001)
        );
    }

    #[test]
    fn ignores_other_sei() {
        let nal = [0x06, 0x01, 0x01, 0x00, 0x80]; // pic_timing-ish
        assert_eq!(parse_lkts(Codec::H264, &nal), None);
        assert_eq!(parse_lkts(Codec::H264, &[0x65, 0x88]), None);
    }
}
