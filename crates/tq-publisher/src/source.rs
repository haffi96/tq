//! TCP camera source: encoder byte stream -> frames on the camera's track.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{io::AsyncReadExt, net::TcpStream};
use tq_media::{AuAssembler, NalFramer, Packager};
use tq_proto::{now_us, FrameHeader, MediaFrame, FLAG_CAPTURE_FROM_SEI, FLAG_KEY};
use tq_transport::track::Track;

use crate::{config::CameraConfig, edges::Edges};

/// Counters shared with the status reporter.
#[derive(Default)]
pub struct CameraStats {
    pub online: AtomicBool,
    pub frames: AtomicU64,
    pub groups: AtomicU64,
    pub bytes: AtomicU64,
    /// Dropped on the uplink (late / superseded), updated by the sender.
    pub dropped: AtomicU64,
    pub capture_lat_sum_us: AtomicU64,
    pub capture_lat_n: AtomicU64,
    pub codec_string: Mutex<Option<String>>,
    pub detected_codec: Mutex<Option<String>>,
}

pub struct Camera {
    pub index: u16,
    pub config: CameraConfig,
    pub track: Arc<Track>,
    pub stats: Arc<CameraStats>,
    pub edges: Arc<Edges>,
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
                if camera.stats.online.swap(false, Relaxed) {
                    camera.edges.emit(tq_proto::Event::TrackOffline {
                        camera: cfg.id.clone(),
                        reason,
                    });
                }
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
                *camera.stats.detected_codec.lock().unwrap() = Some(codec.to_string());
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
                camera.stats.groups.fetch_add(1, Relaxed);
                let mut cs = camera.stats.codec_string.lock().unwrap();
                if cs.as_deref() != Some(&frame.codec_string) {
                    tracing::info!(camera = %cfg.id, codec = %frame.codec_string, "codec string");
                    *cs = Some(frame.codec_string.clone());
                }
            }
            if !started {
                started = true;
                if !camera.stats.online.swap(true, Relaxed) {
                    camera
                        .edges
                        .emit(tq_proto::Event::TrackOnline { camera: cfg.id.clone() });
                }
            }
            let mut flags = if frame.key { FLAG_KEY } else { 0 };
            let capture_us = match frame.capture_us {
                Some(ts) => {
                    flags |= FLAG_CAPTURE_FROM_SEI;
                    camera
                        .stats
                        .capture_lat_sum_us
                        .fetch_add(publisher_us.saturating_sub(ts), Relaxed);
                    camera.stats.capture_lat_n.fetch_add(1, Relaxed);
                    ts
                }
                None => publisher_us,
            };
            camera.stats.frames.fetch_add(1, Relaxed);
            camera.stats.bytes.fetch_add(frame.data.len() as u64, Relaxed);
            let media = MediaFrame {
                camera: camera.index,
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
            camera.track.push(media, camera.index);
        }
    }
}
