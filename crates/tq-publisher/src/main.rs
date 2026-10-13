//! tq-publisher: runs on the vehicle. Reads each camera's pre-encoded
//! H.264/H.265 TCP stream, sends it to the relay one QUIC stream per GOP,
//! and applies teleop commands from the operator behind a dead-man switch.

mod config;
mod source;
mod teleop;
mod uplink;

use std::{path::PathBuf, sync::Arc, time::Duration};

use clap::{CommandFactory, FromArgMatches, Parser};

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
    let build = tq_proto::build_info!("tq-publisher");
    let version: &'static str = Box::leak(build.long().into_boxed_str());
    let args = Args::from_arg_matches(&Args::command().version(version).get_matches()).unwrap_or_else(|e| e.exit());
    let mut cfg = config::Config::load(&args.config)?;
    if let Some(url) = args.relay_url {
        cfg.relay_url = url;
    }
    tracing::info!(%build, av_id = %cfg.av_id, relay = %cfg.relay_url, cameras = cfg.cameras.len(), "starting publisher");

    let cameras: Vec<Arc<source::Camera>> = cfg
        .cameras
        .iter()
        .enumerate()
        .map(|(i, c)| {
            Arc::new(source::Camera {
                index: i as u16,
                config: c.clone(),
                track: tq_transport::track::Track::new(),
                stats: Default::default(),
            })
        })
        .collect();
    for cam in &cameras {
        tokio::spawn(source::run(cam.clone()));
    }

    let teleop = teleop::Teleop::new(cfg.teleop.clone(), Arc::new(teleop::LoggingSink));
    let ticker = teleop.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_millis(20));
        loop {
            t.tick().await;
            ticker.tick();
        }
    });

    let uplink = Arc::new(uplink::Uplink {
        build,
        cfg,
        cameras,
        teleop,
    });
    tokio::select! {
        _ = uplink.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
