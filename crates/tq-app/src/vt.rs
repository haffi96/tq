//! Hardware H.264 / H.265 decoding with VideoToolbox.
//!
//! Frames arrive as Annex B access units with in-band parameter sets on
//! every keyframe. The decoder builds a `CMVideoFormatDescription` from those
//! parameter sets, rewrites each access unit into 4-byte length-prefixed
//! NALs (what VideoToolbox takes), and decodes into IOSurface-backed NV12
//! full-range `CVPixelBuffer`s. That is the format gpui's `surface()`
//! element draws without a copy.
//!
//! Decoding is synchronous: VideoToolbox only reorders frames when temporal
//! processing is requested, and the streams have no B-frames, so the output
//! callback runs inside `decode()`, one call per frame.

#![allow(non_upper_case_globals, non_snake_case)]

use std::{ffi::c_void, ptr, ptr::NonNull};

use anyhow::{bail, Context};
use core_foundation::{
    base::{kCFAllocatorDefault, CFAllocatorRef, CFRelease, CFType, CFTypeRef, OSStatus, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, CVPixelBuffer};
use tq_media::{Codec, NalInfo};

type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type VTDecompressionSessionRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CMTIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image: *mut CVPixelBuffer,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_header_length: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_header_length: i32,
        extensions: CFDictionaryRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source: *const c_void,
        destination: CMBlockBufferRef,
        offset: usize,
        length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        num_samples: isize,
        num_timing: isize,
        timing: *const CMSampleTimingInfo,
        num_sizes: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTDecompressionPropertyKey_RealTime: CFStringRef;
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        format: CMFormatDescriptionRef,
        decoder_specification: CFDictionaryRef,
        image_buffer_attributes: CFDictionaryRef,
        callback: *const VTDecompressionOutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionCanAcceptFormatDescription(
        session: VTDecompressionSessionRef,
        format: CMFormatDescriptionRef,
    ) -> u8;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    fn VTSessionSetProperty(session: *const c_void, key: CFStringRef, value: CFTypeRef) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
}

/// `kVTDecodeInfo_FrameDropped`.
const INFO_FRAME_DROPPED: u32 = 1 << 1;

/// A decoded picture. CoreFoundation objects are reference counted
/// atomically, so handing one to the UI thread is fine.
#[derive(Clone)]
pub struct Picture(pub CFRetained<CVPixelBuffer>);

impl Picture {
    pub fn size(&self) -> (usize, usize) {
        use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
        (CVPixelBufferGetWidth(&self.0), CVPixelBufferGetHeight(&self.0))
    }
}

unsafe impl Send for Picture {}
unsafe impl Sync for Picture {}

/// Where the output callback leaves its picture; lives on the heap so its
/// address is stable for VideoToolbox.
#[derive(Default)]
struct Slot {
    picture: Option<Picture>,
    status: OSStatus,
}

pub struct Decoder {
    codec: Codec,
    /// Parameter sets (without start codes) of the current format.
    params: Vec<Vec<u8>>,
    format: CMFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    slot: Box<Slot>,
}

// The raw CoreMedia / VideoToolbox handles are only used from the thread
// that owns the decoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            params: Vec::new(),
            format: ptr::null(),
            session: ptr::null(),
            slot: Box::default(),
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Decodes one Annex B access unit. Returns the picture, or `None` when
    /// VideoToolbox dropped the frame. Delta frames before the first
    /// keyframe are an error.
    pub fn decode(&mut self, annexb: &[u8], key: bool) -> anyhow::Result<Option<Picture>> {
        let mut params = Vec::new();
        let mut sample = Vec::with_capacity(annexb.len() + 16);
        for nal in split_annexb(annexb) {
            let Some(info) = NalInfo::parse(self.codec, nal) else {
                continue;
            };
            if info.is_vps() || info.is_sps() || info.is_pps() {
                params.push(nal.to_vec());
            } else if !info.is_aud() {
                sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                sample.extend_from_slice(nal);
            }
        }
        if key && !params.is_empty() && params != self.params {
            self.set_format(params)?;
        }
        if self.session.is_null() {
            bail!("no keyframe yet");
        }
        if sample.is_empty() {
            return Ok(None);
        }
        unsafe { self.decode_sample(&sample) }
    }

    fn set_format(&mut self, params: Vec<Vec<u8>>) -> anyhow::Result<()> {
        // VideoToolbox wants VPS, SPS, PPS in that order.
        let rank = |p: &Vec<u8>| {
            let i = NalInfo::parse(self.codec, p).unwrap();
            (!i.is_vps(), !i.is_sps())
        };
        let mut params = params;
        params.sort_by_key(rank);
        let pointers: Vec<*const u8> = params.iter().map(|p| p.as_ptr()).collect();
        let sizes: Vec<usize> = params.iter().map(|p| p.len()).collect();
        let mut format: CMFormatDescriptionRef = ptr::null();
        let status = unsafe {
            match self.codec {
                Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    kCFAllocatorDefault,
                    params.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut format,
                ),
                Codec::H265 => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    kCFAllocatorDefault,
                    params.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    ptr::null(),
                    &mut format,
                ),
            }
        };
        if status != 0 {
            bail!("CMVideoFormatDescriptionCreate: {status}");
        }
        unsafe {
            if !self.session.is_null() && VTDecompressionSessionCanAcceptFormatDescription(self.session, format) == 0 {
                self.close_session();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
        self.format = format;
        self.params = params;
        if self.session.is_null() {
            unsafe { self.open_session() }?;
        }
        Ok(())
    }

    unsafe fn open_session(&mut self) -> anyhow::Result<()> {
        let attrs = CFDictionary::<CFString, CFType>::from_CFType_pairs(&[
            (
                CFString::wrap_under_get_rule(kCVPixelBufferPixelFormatTypeKey),
                CFNumber::from(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i32).as_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCVPixelBufferMetalCompatibilityKey),
                CFBoolean::true_value().as_CFType(),
            ),
            (
                CFString::wrap_under_get_rule(kCVPixelBufferIOSurfacePropertiesKey),
                CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]).as_CFType(),
            ),
        ]);
        let record = VTDecompressionOutputCallbackRecord {
            callback: on_output,
            refcon: &mut *self.slot as *mut Slot as *mut c_void,
        };
        let mut session: VTDecompressionSessionRef = ptr::null();
        let status = VTDecompressionSessionCreate(
            kCFAllocatorDefault,
            self.format,
            ptr::null(),
            attrs.as_concrete_TypeRef(),
            &record,
            &mut session,
        );
        if status != 0 {
            bail!("VTDecompressionSessionCreate: {status}");
        }
        VTSessionSetProperty(
            session,
            kVTDecompressionPropertyKey_RealTime,
            CFBoolean::true_value().as_CFTypeRef(),
        );
        self.session = session;
        Ok(())
    }

    unsafe fn close_session(&mut self) {
        if !self.session.is_null() {
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            self.session = ptr::null();
        }
    }

    unsafe fn decode_sample(&mut self, data: &[u8]) -> anyhow::Result<Option<Picture>> {
        let mut block: CMBlockBufferRef = ptr::null();
        let status = CMBlockBufferCreateWithMemoryBlock(
            kCFAllocatorDefault,
            ptr::null_mut(),
            data.len(),
            kCFAllocatorDefault,
            ptr::null(),
            0,
            data.len(),
            0,
            &mut block,
        );
        if status != 0 {
            bail!("CMBlockBufferCreateWithMemoryBlock: {status}");
        }
        let status = CMBlockBufferReplaceDataBytes(data.as_ptr() as *const c_void, block, 0, data.len());
        if status != 0 {
            CFRelease(block);
            bail!("CMBlockBufferReplaceDataBytes: {status}");
        }
        let timing = CMSampleTimingInfo {
            duration: CMTIME_INVALID,
            presentation_time_stamp: CMTIME_INVALID,
            decode_time_stamp: CMTIME_INVALID,
        };
        let size = data.len();
        let mut sample: CMSampleBufferRef = ptr::null();
        let status =
            CMSampleBufferCreateReady(kCFAllocatorDefault, block, self.format, 1, 1, &timing, 1, &size, &mut sample);
        CFRelease(block);
        if status != 0 {
            bail!("CMSampleBufferCreateReady: {status}");
        }
        *self.slot = Slot::default();
        let mut info = 0u32;
        let status = VTDecompressionSessionDecodeFrame(self.session, sample, 0, ptr::null_mut(), &mut info);
        CFRelease(sample);
        if status != 0 {
            bail!("VTDecompressionSessionDecodeFrame: {status}");
        }
        if self.slot.status != 0 {
            bail!("decode callback: {}", self.slot.status);
        }
        if info & INFO_FRAME_DROPPED != 0 {
            return Ok(None);
        }
        Ok(self.slot.picture.take())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            self.close_session();
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
    }
}

extern "C" fn on_output(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image: *mut CVPixelBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let slot = unsafe { &mut *(refcon as *mut Slot) };
    slot.status = status;
    if status == 0 {
        if let Some(image) = NonNull::new(image) {
            // The callback does not own the image: retain it.
            slot.picture = Some(Picture(unsafe { CFRetained::retain(image) }));
        }
    }
}

/// NAL units of an Annex B buffer, without their start codes.
pub fn split_annexb(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&(sc, _)| sc);
        // A four-byte start code leaves its leading zero on the previous NAL.
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            nals.push(&data[begin..end]);
        }
    }
    nals.into_iter()
}

/// Codec of a group from its WebCodecs codec string.
pub fn codec_of(codec_string: &str) -> anyhow::Result<Codec> {
    if codec_string.starts_with("avc1") || codec_string.starts_with("avc3") {
        Ok(Codec::H264)
    } else if codec_string.starts_with("hev1") || codec_string.starts_with("hvc1") {
        Ok(Codec::H265)
    } else {
        Err(anyhow::anyhow!("unsupported codec {codec_string}")).context("codec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 0];
        let nals: Vec<&[u8]> = split_annexb(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
    }
}
