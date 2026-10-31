//! tq-publisher: runs on the robot. Publishes a broadcast: each camera's
//! pre-encoded H.264/H.265 TCP stream as a video track (one QUIC stream
//! per GOP, never re-encoded), plus teleop: drive commands behind a
//! dead-man watchdog, e-stop, and the robot's status.

mod config;
mod source;
mod uplink;

use std::{path::PathBuf, sync::Arc};

use clap::{CommandFactory, FromArgMatches, Parser};
use tq_wire::{Advertisement, TrackInfo, VideoInfo};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Publisher config (JSON); see config/publisher.example.json.
    #[arg(long)]
    config: PathBuf,
    /// Override relay_url from the config.
    #[arg(long)]
    relay_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();
    let build = tq_wire::build_info!("tq-publisher");
    let version: &'static str = Box::leak(build.long().into_boxed_str());
    let args = Args::from_arg_matches(&Args::command().version(version).get_matches()).unwrap_or_else(|e| e.exit());
    let mut cfg = config::Config::load(&args.config)?;
    if let Some(url) = args.relay_url {
        cfg.relay_url = url;
    }
    tracing::info!(%build, room = %cfg.room_id, broadcast = %cfg.broadcast, relay = %cfg.relay_url, cameras = cfg.cameras.len(), "starting publisher");

    let mut ad = Advertisement {
        broadcast: cfg.broadcast.clone(),
        tracks: cfg
            .cameras
            .iter()
            .map(|c| {
                let mut t = TrackInfo::video(
                    &c.id,
                    VideoInfo {
                        codec: c.codec.clone(),
                        width: c.width,
                        height: c.height,
                        fps: c.fps,
                    },
                );
                t.label = c.label.clone();
                t
            })
            .collect(),
        ..Default::default()
    };
    tq_teleop::advertise(&mut ad, &cfg.teleop);
    let (publication, inbox) = tq::Publication::new(ad)?;

    let cameras: Vec<Arc<source::Camera>> = cfg
        .cameras
        .iter()
        .map(|c| {
            Arc::new(source::Camera {
                track: publication.video(&c.id).expect("advertised above"),
                config: c.clone(),
                publication: publication.clone(),
                frames: Default::default(),
                detected_codec: Default::default(),
            })
        })
        .collect();
    for cam in &cameras {
        tokio::spawn(source::run(cam.clone()));
    }

    let teleop = tq_teleop::Teleop::new(&publication, Arc::new(tq_teleop::LoggingSink))?;
    tokio::spawn(uplink::teleop(teleop.clone(), inbox));
    tokio::spawn(uplink::status(publication.clone(), teleop, cameras, cfg.status_interval_ms));
    tokio::spawn(uplink::keyframe_requests(publication.clone()));

    let uplink = uplink::Uplink { build, cfg, publication };
    tokio::select! {
        _ = uplink.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
