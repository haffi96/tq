// H.264 SPS rewriting for low-latency decoding (ported from hybrid-bridge's
// webcodecs-viewer branch).
//
// Without VUI bitstream_restriction a decoder must assume frames can be
// reordered and hold up to max_dec_frame_buffering of them before output.
// Chrome's VideoToolbox WebCodecs decoder does exactly that: a vtenc_h264_hw
// stream (POC type 0, no bitstream_restriction) is shown ~5 frames late. The
// publisher never reorders (no B-frames), so bitstream_restriction with
// max_num_reorder_frames=0 is added to the SPS before it reaches the
// decoder. An SPS that already carries bitstream_restriction (x264) is left
// alone. H.265 needs nothing: sps_max_num_reorder_pics is mandatory there.

import { joinNalUnits, nalUnits, sameBytes } from './annexb';

class BitReader {
  pos = 0;
  constructor(readonly bytes: Uint8Array) {}
  bits(n: number): number {
    let v = 0;
    for (let i = 0; i < n; i += 1) {
      const byte = this.bytes[this.pos >> 3];
      if (byte === undefined) throw new Error('SPS truncated');
      v = v * 2 + ((byte >> (7 - (this.pos & 7))) & 1);
      this.pos += 1;
    }
    return v;
  }
  flag(): boolean {
    return this.bits(1) === 1;
  }
  ue(): number {
    let zeros = 0;
    while (this.bits(1) === 0) {
      zeros += 1;
      if (zeros > 31) throw new Error('bad exp-Golomb code');
    }
    return 2 ** zeros - 1 + this.bits(zeros);
  }
  se(): number {
    const k = this.ue();
    return k & 1 ? (k + 1) / 2 : -(k / 2);
  }
}

class BitWriter {
  private out: number[] = [];
  private cur = 0;
  private n = 0;
  bit(b: number) {
    this.cur = (this.cur << 1) | b;
    this.n += 1;
    if (this.n === 8) {
      this.out.push(this.cur);
      this.cur = 0;
      this.n = 0;
    }
  }
  bits(v: number, n: number) {
    for (let i = n - 1; i >= 0; i -= 1) this.bit(Math.floor(v / 2 ** i) & 1);
  }
  ue(v: number) {
    const len = Math.floor(Math.log2(v + 1));
    this.bits(0, len);
    this.bits(v + 1, len + 1);
  }
  // Copies bits [from, to) of the reader's buffer.
  copy(reader: BitReader, from: number, to: number) {
    const r = new BitReader(reader.bytes);
    r.pos = from;
    while (r.pos < to) this.bit(r.bits(1));
  }
  // rbsp_trailing_bits
  finish(): Uint8Array {
    this.bit(1);
    while (this.n !== 0) this.bit(0);
    return Uint8Array.from(this.out);
  }
}

function unescapeRBSP(nal: Uint8Array): Uint8Array {
  const out: number[] = [];
  let zeros = 0;
  for (const b of nal) {
    if (zeros >= 2 && b === 3) {
      zeros = 0;
      continue;
    }
    zeros = b === 0 ? zeros + 1 : 0;
    out.push(b);
  }
  return Uint8Array.from(out);
}

function escapeRBSP(rbsp: Uint8Array): Uint8Array {
  const out: number[] = [];
  let zeros = 0;
  for (const byte of rbsp) {
    if (zeros >= 2 && byte <= 3) {
      out.push(3);
      zeros = 0;
    }
    out.push(byte);
    zeros = byte === 0 ? zeros + 1 : 0;
  }
  return Uint8Array.from(out);
}

function skipScalingList(r: BitReader, size: number) {
  let last = 8;
  let next = 8;
  for (let j = 0; j < size; j += 1) {
    if (next !== 0) next = (last + r.se() + 256) % 256;
    last = next === 0 ? last : next;
  }
}

function skipHRD(r: BitReader) {
  const count = r.ue() + 1;
  r.bits(4); // bit_rate_scale
  r.bits(4); // cpb_size_scale
  for (let i = 0; i < count; i += 1) {
    r.ue(); // bit_rate_value_minus1
    r.ue(); // cpb_size_value_minus1
    r.bits(1); // cbr_flag
  }
  r.bits(5 * 4); // four 5-bit length fields
}

const HIGH_PROFILES = new Set([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135]);

// Returns the SPS NAL (header byte included, emulation-prevented) with
// bitstream_restriction added, or null when it already has one or cannot be
// parsed (the caller then keeps the original).
export function addNoReorderToSPS(nal: Uint8Array): Uint8Array | null {
  if (!nal.length || (nal[0]! & 0x1f) !== 7) return null;
  const rbsp = unescapeRBSP(nal.subarray(1));
  const r = new BitReader(rbsp);
  try {
    const profileIdc = r.bits(8);
    r.bits(16); // constraint flags, level_idc
    r.ue(); // seq_parameter_set_id
    if (HIGH_PROFILES.has(profileIdc)) {
      const chroma = r.ue();
      if (chroma === 3) r.bits(1);
      r.ue(); // bit_depth_luma_minus8
      r.ue(); // bit_depth_chroma_minus8
      r.bits(1); // qpprime_y_zero_transform_bypass_flag
      if (r.flag()) {
        for (let i = 0; i < (chroma === 3 ? 12 : 8); i += 1) {
          if (r.flag()) skipScalingList(r, i < 6 ? 16 : 64);
        }
      }
    }
    r.ue(); // log2_max_frame_num_minus4
    const pocType = r.ue();
    if (pocType === 0) {
      r.ue();
    } else if (pocType === 1) {
      r.bits(1);
      r.se();
      r.se();
      const n = r.ue();
      for (let i = 0; i < n; i += 1) r.se();
    }
    const maxNumRefFrames = r.ue();
    r.bits(1); // gaps_in_frame_num_value_allowed_flag
    r.ue(); // pic_width_in_mbs_minus1
    r.ue(); // pic_height_in_map_units_minus1
    if (!r.flag()) r.bits(1); // frame_mbs_only_flag, mb_adaptive_frame_field_flag
    r.bits(1); // direct_8x8_inference_flag
    if (r.flag()) {
      r.ue();
      r.ue();
      r.ue();
      r.ue();
    } // frame cropping

    const w = new BitWriter();
    const vuiFlagPos = r.pos;
    const restriction = (writer: BitWriter) => {
      writer.bit(1); // bitstream_restriction_flag
      writer.bit(1); // motion_vectors_over_pic_boundaries_flag
      writer.ue(2); // max_bytes_per_pic_denom (default)
      writer.ue(1); // max_bits_per_mb_denom (default)
      writer.ue(16); // log2_max_mv_length_horizontal (default)
      writer.ue(16); // log2_max_mv_length_vertical (default)
      writer.ue(0); // max_num_reorder_frames: output every frame immediately
      writer.ue(Math.max(maxNumRefFrames, 1)); // max_dec_frame_buffering
    };

    if (!r.flag()) {
      // No VUI: write one that carries only bitstream_restriction.
      w.copy(r, 0, vuiFlagPos);
      w.bit(1); // vui_parameters_present_flag
      w.bits(0, 8); // aspect_ratio, overscan, video_signal_type, chroma_loc,
      //                timing_info, nal_hrd, vcl_hrd, pic_struct_present
      restriction(w);
    } else {
      if (r.flag() && r.bits(8) === 255) r.bits(32); // aspect_ratio_info (Extended_SAR)
      if (r.flag()) r.bits(1); // overscan
      if (r.flag()) {
        r.bits(4);
        if (r.flag()) r.bits(24);
      } // video_signal_type, colour description
      if (r.flag()) {
        r.ue();
        r.ue();
      } // chroma_loc_info
      if (r.flag()) r.bits(65); // timing_info: two u(32) + fixed_frame_rate_flag
      const nalHRD = r.flag();
      if (nalHRD) skipHRD(r);
      const vclHRD = r.flag();
      if (vclHRD) skipHRD(r);
      if (nalHRD || vclHRD) r.bits(1); // low_delay_hrd_flag
      r.bits(1); // pic_struct_present_flag
      const restrictionPos = r.pos;
      if (r.flag()) return null; // already restricted; trust the encoder
      w.copy(r, 0, restrictionPos);
      restriction(w);
    }
    const body = escapeRBSP(w.finish());
    const out = new Uint8Array(body.length + 1);
    out[0] = nal[0]!;
    out.set(body, 1);
    return out;
  } catch {
    return null;
  }
}

// Rewrites every SPS in a keyframe access unit, caching the last rewrite
// so it runs once per stream rather than on every IDR.
export class SpsPatcher {
  private spsIn: Uint8Array | null = null;
  private spsOut: Uint8Array | null = null;

  get patched(): boolean {
    return this.spsOut !== null;
  }

  apply(data: Uint8Array): Uint8Array {
    const nals = nalUnits(data);
    let changed = false;
    const rewritten = nals.map((nal) => {
      if ((nal[0]! & 0x1f) !== 7) return nal;
      if (!this.spsIn || !sameBytes(nal, this.spsIn)) {
        this.spsIn = nal.slice();
        this.spsOut = addNoReorderToSPS(nal);
      }
      if (!this.spsOut) return nal;
      changed = true;
      return this.spsOut;
    });
    return changed ? joinNalUnits(rewritten) : data;
  }
}
