//! TCP video source: an encoder's byte stream -> frames on a video track.
//! Anything that writes H.264/H.265 (Annex B or length-prefixed) to a TCP
//! socket is a camera.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use tokio::{io::AsyncReadExt, net::TcpStream};
use tq::Publication;
use tq_media::{AuAssembler, Codec, Framing, FramingMode, NalFramer, Packager};
use tq_transport::track::Track;
use tq_wire::{now_us, FrameHeader, MediaFrame, FLAG_CAPTURE_FROM_SEI, FLAG_KEY};

use super::{SourceConfig, Stats};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub struct Options {
    addr: String,
    codec: Option<Codec>,
    framing: FramingMode,
    complete_on_vcl: bool,
}

impl Options {
    pub fn from_config(c: &SourceConfig) -> anyhow::Result<Self> {
        let SourceConfig::Tcp {
            addr,
            codec,
            framing,
            complete_on_vcl,
        } = c;
        let codec = match codec.as_deref().unwrap_or("auto") {
            "auto" => None,
            c => Some(c.parse().map_err(anyhow::Error::msg)?),
        };
        let framing = match framing.as_deref().unwrap_or("auto") {
            "auto" => FramingMode::Auto,
            "annexb" | "byte-stream" => FramingMode::Fixed(Framing::AnnexB),
            "length_prefixed" | "avc" => FramingMode::Fixed(Framing::LengthPrefixed),
            other => anyhow::bail!("unknown video framing '{other}' (auto, annexb, length_prefixed)"),
        };
        Ok(Self {
            addr: addr.clone(),
            codec,
            framing,
            complete_on_vcl: *complete_on_vcl,
        })
    }
}

struct Camera {
    id: String,
    opts: Options,
    track: Arc<Track>,
    publication: Arc<Publication>,
    stats: Arc<Stats>,
}

/// Reads the encoder's TCP stream forever, reconnecting when it drops.
pub async fn run(id: String, opts: Options, track: Arc<Track>, publication: Arc<Publication>, stats: Arc<Stats>) {
    let camera = Camera {
        id,
        opts,
        track,
        publication,
        stats,
    };
    let (camera, cfg) = (&camera, &camera.opts);
    let mut logged_down = false;
    // Group numbers only need to be distinct per source connection; the
    // track renumbers them.
    let mut group: u64 = 0;
    loop {
        match TcpStream::connect(&cfg.addr).await {
            Ok(stream) => {
                logged_down = false;
                let _ = stream.set_nodelay(true);
                tracing::info!(track = %camera.id, source = %cfg.addr, "source connected");
                let reason = if let Err(e) = read_stream(camera, stream, &mut group).await {
                    tracing::warn!(track = %camera.id, error = %e, "source error");
                    format!("source error: {e}")
                } else {
                    tracing::warn!(track = %camera.id, "source closed");
                    "source closed".to_string()
                };
                camera.publication.set_online(&camera.id, false, &reason);
                camera.track.reset();
            }
            Err(e) => {
                if !logged_down {
                    tracing::warn!(track = %camera.id, source = %cfg.addr, error = %e, "source unavailable, retrying every second");
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn read_stream(camera: &Camera, mut stream: TcpStream, group: &mut u64) -> anyhow::Result<()> {
    let cfg = &camera.opts;
    let mut framer = NalFramer::new(cfg.framing, cfg.codec);
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
                tracing::info!(track = %camera.id, %codec, framing = ?framer.framing().unwrap(), "stream detected");
                *camera.stats.codec.lock().unwrap() = Some(codec.to_string());
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
                    tracing::info!(track = %camera.id, codec = %frame.codec_string, "codec string");
                    codec_string = Some(frame.codec_string.clone());
                }
            }
            if !started {
                started = true;
                camera.publication.set_online(&camera.id, true, "");
            }
            let mut flags = if frame.key { FLAG_KEY } else { 0 };
            let capture_us = match frame.capture_us {
                Some(ts) => {
                    flags |= FLAG_CAPTURE_FROM_SEI;
                    ts
                }
                None => publisher_us,
            };
            camera.stats.frames.fetch_add(1, Relaxed);
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
