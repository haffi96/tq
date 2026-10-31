//! TCP video source: an encoder's byte stream -> frames on a video track.
//! Anything that writes H.264/H.265 (Annex B or length-prefixed) to a TCP
//! socket is a camera.

use std::{
    sync::{atomic::Ordering::Relaxed, Arc},
    time::Duration,
};

use tokio::{io::AsyncReadExt, net::TcpStream};
use tq::{
    media::{Framing, FramingMode},
    VideoOptions, VideoWriter,
};

use super::{SourceConfig, Stats};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

pub struct Options {
    pub video: VideoOptions,
}

impl Options {
    pub fn from_config(c: &SourceConfig) -> anyhow::Result<Self> {
        let SourceConfig::Tcp {
            codec,
            framing,
            complete_on_vcl,
            ..
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
            video: VideoOptions {
                codec,
                framing,
                complete_on_vcl: *complete_on_vcl,
            },
        })
    }
}

/// Reads the encoder's TCP stream forever, reconnecting when it drops.
pub async fn run(addr: String, mut out: VideoWriter, stats: Arc<Stats>) {
    let mut logged_down = false;
    loop {
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                logged_down = false;
                let _ = stream.set_nodelay(true);
                tracing::info!(track = %out.id(), source = %addr, "source connected");
                let reason = match read_stream(&mut out, stream, &stats).await {
                    Ok(()) => {
                        tracing::warn!(track = %out.id(), "source closed");
                        "source closed".to_string()
                    }
                    Err(e) => {
                        tracing::warn!(track = %out.id(), error = %e, "source error");
                        format!("source error: {e}")
                    }
                };
                out.reset(&reason);
            }
            Err(e) => {
                if !logged_down {
                    tracing::warn!(track = %out.id(), source = %addr, error = %e, "source unavailable, retrying every second");
                    logged_down = true;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn read_stream(out: &mut VideoWriter, mut stream: TcpStream, stats: &Stats) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        out.write(&buf[..n])?;
        stats.frames.store(out.frames(), Relaxed);
        if let Some(codec) = out.codec() {
            let mut c = stats.codec.lock().unwrap();
            if c.is_none() {
                *c = Some(codec.to_string());
            }
        }
    }
}
