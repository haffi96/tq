//! TCP camera source: encoder byte stream -> frames on the camera's video
//! track. Anything that writes H.264/H.265 (Annex B or length-prefixed) to
//! a TCP socket is a camera.

use std::{
    sync::{atomic::AtomicU64, atomic::Ordering::Relaxed, Arc, Mutex},
    time::Duration,
};

use tokio::{io::AsyncReadExt, net::TcpStream};
use tq::Publication;
use tq_media::{AuAssembler, NalFramer, Packager};
use tq_transport::track::Track;
use tq_wire::{now_us, FrameHeader, MediaFrame, FLAG_CAPTURE_FROM_SEI, FLAG_KEY};

use crate::config::CameraConfig;

pub struct Camera {
    pub config: CameraConfig,
    /// The camera's video track in the publication.
    pub track: Arc<Track>,
    pub publication: Arc<Publication>,
    pub frames: AtomicU64,
    pub detected_codec: Mutex<Option<String>>,
}

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Reads the camera's TCP stream forever, reconnecting when it drops.
pub async fn run(camera: Arc<Camera>) {
    let cfg = &camera.config;
    let mut logged_down = false;
    // Group numbers only need to be distinct per source connection; the
    // track renumbers them.
    let mut group: u64 = 0;
    loop {
        match TcpStream::connect(&cfg.source).await {
            Ok(stream) => {
                logged_down = false;
                let _ = stream.set_nodelay(true);
                tracing::info!(camera = %cfg.id, source = %cfg.source, "source connected");
                let reason = if let Err(e) = read_stream(&camera, stream, &mut group).await {
                    tracing::warn!(camera = %cfg.id, error = %e, "source error");
                    format!("source error: {e}")
                } else {
                    tracing::warn!(camera = %cfg.id, "source closed");
                    "source closed".to_string()
                };
                camera.publication.set_online(&cfg.id, false, &reason);
                camera.track.reset();
            }
            Err(e) => {
                if !logged_down {
                    tracing::warn!(camera = %cfg.id, source = %cfg.source, error = %e, "source unavailable, retrying every second");
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn read_stream(camera: &Camera, mut stream: TcpStream, group: &mut u64) -> anyhow::Result<()> {
    let cfg = &camera.config;
    let mut framer = NalFramer::new(cfg.framing()?, cfg.codec()?);
    let mut pipeline: Option<(AuAssembler, Packager)> = None;
    let mut index: u32 = 0;
    let mut started = false;
    let mut codec_string: Option<String> = None;
    let mut buf = vec![0u8; 256 << 10];
    let mut nals = Vec::new();
    let mut aus = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        nals.clear();
        framer.push(&buf[..n], &mut nals)?;
        if nals.is_empty() {
            continue;
        }
        let (assembler, packager) = match pipeline.as_mut() {
            Some(p) => p,
            None => {
                let codec = framer.codec().expect("codec known once a NAL is out");
                tracing::info!(camera = %cfg.id, %codec, framing = ?framer.framing().unwrap(), "stream detected");
                *camera.detected_codec.lock().unwrap() = Some(codec.to_string());
                pipeline.insert((AuAssembler::new(codec, cfg.complete_on_vcl), Packager::new(codec)))
            }
        };
        aus.clear();
        for nal in nals.drain(..) {
            assembler.push(nal, &mut aus);
        }
        for au in aus.drain(..) {
            let Some(frame) = packager.package(au) else { continue };
            let publisher_us = now_us();
            if frame.key {
                *group += 1;
                index = 0;
                if codec_string.as_deref() != Some(&frame.codec_string) {
                    tracing::info!(camera = %cfg.id, codec = %frame.codec_string, "codec string");
                    codec_string = Some(frame.codec_string.clone());
                }
            }
            if !started {
                started = true;
                camera.publication.set_online(&cfg.id, true, "");
            }
            let mut flags = if frame.key { FLAG_KEY } else { 0 };
            let capture_us = match frame.capture_us {
                Some(ts) => {
                    flags |= FLAG_CAPTURE_FROM_SEI;
                    ts
                }
                None => publisher_us,
            };
            camera.frames.fetch_add(1, Relaxed);
            let media = MediaFrame {
                // The session stamps its own indices.
                broadcast: 0,
                track: 0,
                group: *group,
                codec: frame.codec_string.as_str().into(),
                header: FrameHeader {
                    index,
                    flags,
                    capture_us,
                    publisher_us,
                    relay_us: 0,
                    frame_id: frame.frame_id.unwrap_or(0),
                },
                payload: frame.data,
            };
            index += 1;
            camera.track.push(media);
        }
    }
}
