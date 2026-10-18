//! Hardware H.264 / H.265 decoding with NVIDIA NVDEC (Linux).
//!
//! The driver's libraries are dlopen'd at runtime: libcuda through cudarc,
//! libnvcuvid through `moq-nvenc`'s cuvid table. A machine without the
//! NVIDIA driver gets a decode error per keyframe, not a crash.
//!
//! Frames arrive as Annex B access units with in-band parameter sets on
//! every keyframe, which is what the cuvid parser takes. The parser runs
//! synchronously: its sequence / decode / display callbacks fire inside
//! `cuvidParseVideoData` on the decoder thread. Each access unit is sent
//! with `CUVID_PKT_ENDOFPICTURE` and the parser has no display delay, so the
//! picture comes back from the same call (the streams have no B-frames).
//!
//! gpui has no zero-copy video surface outside macOS, so the NV12 output is
//! copied to the host and converted to BGRA here, on the decoder thread, and
//! the UI draws it as an image.

use core::ffi::{c_int, c_uint, c_ulong, c_ulonglong, c_void};
use std::{ptr, sync::Arc};

use anyhow::{bail, Context as _};
use cudarc::driver::{sys::CUresult, CudaContext};
use gpui_kit::RenderImage;
use moq_nvenc::{
    cuvid,
    sys::{
        cuviddec::{
            cudaVideoChromaFormat, cudaVideoCodec, cudaVideoCreateFlags, cudaVideoDeinterlaceMode,
            cudaVideoSurfaceFormat, CUvideodecoder, CUVIDDECODECAPS, CUVIDDECODECREATEINFO, CUVIDPICPARAMS,
            CUVIDPROCPARAMS,
        },
        nvcuvid::{
            CUVIDEOFORMAT, CUVIDPARSERDISPINFO, CUVIDPARSERPARAMS, CUVIDSOURCEDATAPACKET, CUvideopacketflags,
            CUvideoparser,
        },
    },
};
use tq_media::Codec;

/// A decoded picture: BGRA, ready for gpui's sprite atlas.
#[derive(Clone)]
pub struct Picture(pub Arc<RenderImage>);

impl Picture {
    pub fn size(&self) -> (usize, usize) {
        let s = self.0.size(0);
        (s.width.0 as usize, s.height.0 as usize)
    }
}

pub struct Decoder {
    codec: Codec,
    /// Opened on the first keyframe, so a missing driver shows up as a
    /// decode error on the camera.
    parser: Option<Parser>,
}

impl Decoder {
    pub fn new(codec: Codec) -> Self {
        Self { codec, parser: None }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Decodes one Annex B access unit. Returns the picture, or `None` when
    /// the parser produced none. Delta frames before the first keyframe are
    /// an error.
    pub fn decode(&mut self, annexb: &[u8], key: bool) -> anyhow::Result<Option<Picture>> {
        if self.parser.is_none() {
            if !key {
                bail!("no keyframe yet");
            }
            self.parser = Some(Parser::open(self.codec)?);
        }
        let parser = self.parser.as_mut().unwrap();
        let result = parser.decode(annexb);
        if result.is_err() {
            // Start over at the next keyframe with a fresh parser.
            self.parser = None;
        }
        result
    }
}

/// The cuvid parser plus the state its callbacks share.
struct Parser {
    handle: CUvideoparser,
    /// Boxed so its address is stable: the parser keeps a pointer to it.
    state: Box<State>,
}

// The parser and decoder handles are only used from the thread that owns
// the decoder; the CUDA context is bound to that thread around each call.
unsafe impl Send for Parser {}

struct State {
    api: &'static cuvid::Api,
    ctx: Arc<CudaContext>,
    decoder: Option<HwDecoder>,
    /// What the display callback queued during the current parse call.
    ready: Vec<CUVIDPARSERDISPINFO>,
    /// The first error inside a callback (callbacks can only return an int).
    error: Option<String>,
    /// Host copy of the mapped NV12 surface, reused between frames.
    nv12: Vec<u8>,
}

/// A cuvid decoder and the output it was created for.
struct HwDecoder {
    api: &'static cuvid::Api,
    /// Destroying the decoder needs its context alive and current.
    ctx: Arc<CudaContext>,
    handle: CUvideodecoder,
    coded: (u32, u32),
    display_area: (i32, i32, i32, i32),
    /// Output size (the display area, even).
    width: u32,
    height: u32,
    yuv: Yuv,
}

impl Drop for HwDecoder {
    fn drop(&mut self) {
        if self.ctx.bind_to_thread().is_ok() {
            unsafe { (self.api.destroy_decoder)(self.handle) };
        }
    }
}

impl Parser {
    fn open(codec: Codec) -> anyhow::Result<Self> {
        // cudarc panics when libcuda is missing; probe it first.
        let libcuda = ["libcuda.so.1", "libcuda.so"]
            .iter()
            .any(|name| unsafe { libloading::Library::new(*name) }.is_ok());
        if !libcuda {
            bail!("NVDEC unavailable: libcuda not found (is the NVIDIA driver installed?)");
        }
        let api = cuvid::Api::get().map_err(|e| anyhow::anyhow!("NVDEC unavailable: {e}"))?;
        let ctx = CudaContext::new(0).map_err(|e| anyhow::anyhow!("CUDA init: {e:?}"))?;
        let mut state = Box::new(State {
            api,
            ctx,
            decoder: None,
            ready: Vec::new(),
            error: None,
            nv12: Vec::new(),
        });
        let mut params = CUVIDPARSERPARAMS {
            CodecType: match codec {
                Codec::H264 => cudaVideoCodec::cudaVideoCodec_H264,
                Codec::H265 => cudaVideoCodec::cudaVideoCodec_HEVC,
            },
            // The sequence callback returns the real number.
            ulMaxNumDecodeSurfaces: 1,
            // Hand each picture back as soon as it is decoded.
            ulMaxDisplayDelay: 0,
            pUserData: &mut *state as *mut State as *mut c_void,
            pfnSequenceCallback: Some(on_sequence),
            pfnDecodePicture: Some(on_decode),
            pfnDisplayPicture: Some(on_display),
            ..Default::default()
        };
        state.ctx.bind_to_thread().map_err(|e| anyhow::anyhow!("CUDA bind: {e:?}"))?;
        let mut handle: CUvideoparser = ptr::null_mut();
        let status = unsafe { (api.create_video_parser)(&mut handle, &mut params) };
        if status != CUresult::CUDA_SUCCESS {
            bail!("cuvidCreateVideoParser: {status:?}");
        }
        tracing::info!(?codec, "opened NVDEC decoder");
        Ok(Self { handle, state })
    }

    fn decode(&mut self, annexb: &[u8]) -> anyhow::Result<Option<Picture>> {
        self.state
            .ctx
            .bind_to_thread()
            .map_err(|e| anyhow::anyhow!("CUDA bind: {e:?}"))?;
        let mut packet = CUVIDSOURCEDATAPACKET {
            flags: CUvideopacketflags::CUVID_PKT_ENDOFPICTURE as c_ulong,
            payload_size: annexb.len() as c_ulong,
            payload: annexb.as_ptr(),
            timestamp: 0,
        };
        let status = unsafe { (self.state.api.parse_video_data)(self.handle, &mut packet) };
        let ready = std::mem::take(&mut self.state.ready);
        if let Some(e) = self.state.error.take() {
            bail!(e);
        }
        if status != CUresult::CUDA_SUCCESS {
            bail!("cuvidParseVideoData: {status:?}");
        }
        // With no display delay there is at most one; keep the newest.
        match ready.last() {
            Some(disp) => self.state.picture(disp).map(Some),
            None => Ok(None),
        }
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        let _ = self.state.ctx.bind_to_thread();
        unsafe { (self.state.api.destroy_video_parser)(self.handle) };
        // The decoder (in `state`) is destroyed after the parser.
    }
}

impl State {
    /// A sequence header (every keyframe repeats it): keep the decoder if
    /// the geometry is unchanged, otherwise create a new one. Returns the
    /// number of decode surfaces for the parser.
    fn sequence(&mut self, f: &CUVIDEOFORMAT) -> Result<c_int, String> {
        if f.chroma_format != cudaVideoChromaFormat::cudaVideoChromaFormat_420 {
            return Err(format!("unsupported chroma format {:?}", f.chroma_format));
        }
        if f.bit_depth_luma_minus8 != 0 || f.bit_depth_chroma_minus8 != 0 {
            return Err(format!("unsupported bit depth {}", f.bit_depth_luma_minus8 + 8));
        }
        let mut caps = CUVIDDECODECAPS {
            eCodecType: f.codec,
            eChromaFormat: f.chroma_format,
            nBitDepthMinus8: 0,
            ..Default::default()
        };
        let status = unsafe { (self.api.get_decoder_caps)(&mut caps) };
        if status != CUresult::CUDA_SUCCESS {
            return Err(format!("cuvidGetDecoderCaps: {status:?}"));
        }
        if caps.bIsSupported == 0 {
            return Err(format!("this GPU cannot decode {:?}", f.codec));
        }
        if f.coded_width > caps.nMaxWidth || f.coded_height > caps.nMaxHeight {
            return Err(format!(
                "{}x{} is over this GPU's {}x{} limit",
                f.coded_width, f.coded_height, caps.nMaxWidth, caps.nMaxHeight
            ));
        }

        let a = &f.display_area;
        let width = (a.right - a.left).max(2) as u32 & !1;
        let height = (a.bottom - a.top).max(2) as u32 & !1;
        let coded = (f.coded_width, f.coded_height);
        let display_area = (a.left, a.top, a.right, a.bottom);
        let surfaces = c_int::from(f.min_num_decode_surfaces.max(1));
        if self
            .decoder
            .as_ref()
            .is_some_and(|d| d.coded == coded && d.display_area == display_area)
        {
            return Ok(surfaces);
        }
        self.decoder = None;

        let mut info = CUVIDDECODECREATEINFO {
            ulWidth: f.coded_width as c_ulong,
            ulHeight: f.coded_height as c_ulong,
            ulNumDecodeSurfaces: surfaces as c_ulong,
            CodecType: f.codec,
            ChromaFormat: f.chroma_format,
            ulCreationFlags: cudaVideoCreateFlags::cudaVideoCreate_PreferCUVID as c_ulong,
            bitDepthMinus8: 0,
            ulMaxWidth: f.coded_width as c_ulong,
            ulMaxHeight: f.coded_height as c_ulong,
            OutputFormat: cudaVideoSurfaceFormat::cudaVideoSurfaceFormat_NV12,
            DeinterlaceMode: cudaVideoDeinterlaceMode::cudaVideoDeinterlaceMode_Weave,
            ulTargetWidth: width as c_ulong,
            ulTargetHeight: height as c_ulong,
            // Each picture is copied out and unmapped before the next one.
            ulNumOutputSurfaces: 2,
            ..Default::default()
        };
        info.display_area.left = a.left as i16;
        info.display_area.top = a.top as i16;
        info.display_area.right = a.right as i16;
        info.display_area.bottom = a.bottom as i16;
        info.target_rect.right = width as i16;
        info.target_rect.bottom = height as i16;
        let mut handle: CUvideodecoder = ptr::null_mut();
        let status = unsafe { (self.api.create_decoder)(&mut handle, &mut info) };
        if status != CUresult::CUDA_SUCCESS {
            return Err(format!("cuvidCreateDecoder: {status:?}"));
        }

        let signal = &f.video_signal_description;
        let yuv = Yuv::new(signal.matrix_coefficients, signal.video_full_range_flag() != 0, height);
        tracing::debug!(width, height, ?yuv, "created cuvid decoder");
        self.decoder = Some(HwDecoder {
            api: self.api,
            ctx: self.ctx.clone(),
            handle,
            coded,
            display_area,
            width,
            height,
            yuv,
        });
        Ok(surfaces)
    }

    /// Maps the decoded surface, copies it to the host and converts it.
    fn picture(&mut self, disp: &CUVIDPARSERDISPINFO) -> anyhow::Result<Picture> {
        let d = self.decoder.as_ref().context("picture before a decoder")?;
        let mut proc_params = CUVIDPROCPARAMS {
            progressive_frame: disp.progressive_frame,
            top_field_first: disp.top_field_first,
            ..Default::default()
        };
        let mut dev_ptr: c_ulonglong = 0;
        let mut pitch: c_uint = 0;
        let status =
            unsafe { (self.api.map_video_frame)(d.handle, disp.picture_index, &mut dev_ptr, &mut pitch, &mut proc_params) };
        if status != CUresult::CUDA_SUCCESS {
            bail!("cuvidMapVideoFrame: {status:?}");
        }
        // NV12 at the target size: `height` rows of luma, then `height / 2`
        // rows of interleaved chroma, each `pitch` bytes.
        let len = pitch as usize * d.height as usize * 3 / 2;
        self.nv12.resize(len, 0);
        let copied = unsafe { cudarc::driver::result::memcpy_dtoh_sync(&mut self.nv12[..], dev_ptr) };
        // Always unmap: the output surfaces are a small fixed pool.
        unsafe { (self.api.unmap_video_frame)(d.handle, dev_ptr) };
        copied.map_err(|e| anyhow::anyhow!("CUDA copy: {e:?}"))?;

        let (w, h) = (d.width as usize, d.height as usize);
        let mut bgra = vec![0u8; w * h * 4];
        d.yuv.nv12_to_bgra(&self.nv12, pitch as usize, w, h, &mut bgra);
        let buffer = image::RgbaImage::from_raw(d.width, d.height, bgra).context("BGRA buffer size")?;
        Ok(Picture(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))))
    }
}

unsafe extern "C" fn on_sequence(user: *mut c_void, format: *mut CUVIDEOFORMAT) -> c_int {
    // Callbacks run inside `parse_video_data` on the decoder thread, while
    // nothing else touches the state.
    let state = unsafe { &mut *(user as *mut State) };
    match state.sequence(unsafe { &*format }) {
        Ok(surfaces) => surfaces,
        Err(e) => {
            state.error.get_or_insert(e);
            0
        }
    }
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut CUVIDPICPARAMS) -> c_int {
    let state = unsafe { &mut *(user as *mut State) };
    let Some(decoder) = &state.decoder else {
        state.error.get_or_insert("picture before a sequence header".into());
        return 0;
    };
    let status = unsafe { (state.api.decode_picture)(decoder.handle, pic) };
    if status != CUresult::CUDA_SUCCESS {
        state.error.get_or_insert(format!("cuvidDecodePicture: {status:?}"));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, disp: *mut CUVIDPARSERDISPINFO) -> c_int {
    let state = unsafe { &mut *(user as *mut State) };
    if !disp.is_null() {
        state.ready.push(unsafe { *disp });
    }
    1
}

/// YCbCr -> RGB for 8-bit video, in 14-bit fixed point.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Yuv {
    /// Black level of luma (16 for limited range).
    y_offset: i32,
    y_gain: i32,
    cr_r: i32,
    cb_g: i32,
    cr_g: i32,
    cb_b: i32,
}

const SHIFT: u32 = 14;

impl Yuv {
    /// From the stream's `matrix_coefficients` (H.273) and range flag.
    /// Unspecified matrices follow the usual convention: BT.709 for HD,
    /// BT.601 for SD.
    fn new(matrix: u8, full_range: bool, height: u32) -> Self {
        let (kr, kb) = match matrix {
            1 => (0.2126, 0.0722),
            5 | 6 => (0.299, 0.114),
            9 | 10 => (0.2627, 0.0593),
            _ if height >= 720 => (0.2126, 0.0722),
            _ => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        let (y_offset, y_scale, c_scale) = if full_range {
            (0, 1.0, 1.0)
        } else {
            (16, 255.0 / 219.0, 255.0 / 224.0)
        };
        let fixed = |v: f64| (v * f64::from(1 << SHIFT)).round() as i32;
        Self {
            y_offset,
            y_gain: fixed(y_scale),
            cr_r: fixed(2.0 * (1.0 - kr) * c_scale),
            cb_g: fixed(2.0 * kb * (1.0 - kb) / kg * c_scale),
            cr_g: fixed(2.0 * kr * (1.0 - kr) / kg * c_scale),
            cb_b: fixed(2.0 * (1.0 - kb) * c_scale),
        }
    }

    /// `nv12`: `h` luma rows then `h / 2` chroma rows, `pitch` bytes each.
    /// `out`: `w * h` BGRA pixels, tightly packed. `w` is even.
    fn nv12_to_bgra(&self, nv12: &[u8], pitch: usize, w: usize, h: usize, out: &mut [u8]) {
        let (luma, chroma) = nv12.split_at(pitch * h);
        let round = 1 << (SHIFT - 1);
        let clamp = |v: i32| ((v + round) >> SHIFT).clamp(0, 255) as u8;
        for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
            let ys = &luma[y * pitch..][..w];
            let uv = &chroma[(y / 2) * pitch..][..w];
            // Two pixels share one chroma sample.
            for ((px, l), c) in row.chunks_exact_mut(8).zip(ys.chunks_exact(2)).zip(uv.chunks_exact(2)) {
                let (cb, cr) = (i32::from(c[0]) - 128, i32::from(c[1]) - 128);
                let (b, g, r) = (self.cb_b * cb, -self.cb_g * cb - self.cr_g * cr, self.cr_r * cr);
                for (px, &l) in px.chunks_exact_mut(4).zip(l) {
                    let l = (i32::from(l) - self.y_offset) * self.y_gain;
                    px.copy_from_slice(&[clamp(l + b), clamp(l + g), clamp(l + r), 255]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(yuv: &Yuv, y: u8, cb: u8, cr: u8) -> [u8; 4] {
        // One 2x2 block: luma rows, then one chroma row.
        let nv12 = [y, y, y, y, cb, cr];
        let mut out = [0u8; 16];
        yuv.nv12_to_bgra(&nv12, 2, 2, 2, &mut out);
        out[..4].try_into().unwrap()
    }

    #[test]
    fn limited_range_bt709() {
        let yuv = Yuv::new(1, false, 1080);
        assert_eq!(convert(&yuv, 16, 128, 128), [0, 0, 0, 255]);
        assert_eq!(convert(&yuv, 235, 128, 128), [255, 255, 255, 255]);
        // BT.709 red: Y 63, Cb 102, Cr 240.
        let [b, g, r, _] = convert(&yuv, 63, 102, 240);
        assert!(r >= 253 && g <= 2 && b <= 2, "{r} {g} {b}");
    }

    #[test]
    fn full_range_bt601() {
        let yuv = Yuv::new(6, true, 480);
        assert_eq!(convert(&yuv, 0, 128, 128), [0, 0, 0, 255]);
        assert_eq!(convert(&yuv, 255, 128, 128), [255, 255, 255, 255]);
        // BT.601 full-range blue: Y 29, Cb 255, Cr 107.
        let [b, g, r, _] = convert(&yuv, 29, 255, 107);
        assert!(b >= 253 && g <= 2 && r <= 2, "{r} {g} {b}");
    }

    #[test]
    fn unspecified_matrix_follows_resolution() {
        assert_eq!(Yuv::new(2, false, 1080), Yuv::new(1, false, 1080));
        assert_eq!(Yuv::new(2, false, 480), Yuv::new(6, false, 480));
    }
}
