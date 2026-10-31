//! tq-edge: the agent on the device. Publishes one broadcast, as its
//! config says: each track fed by a source (an encoder's byte stream, a
//! program's JSON lines), each channel feeding a sink, and optionally
//! teleop: drive commands behind a dead-man lock and the remote stop.
//! Video is never re-encoded: one QUIC stream per GOP.

mod config;
mod sink;
mod source;
mod uplink;

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Config (JSON); see config/edge.example.json.
    #[arg(long)]
    config: PathBuf,
    /// Override relay.url from the config.
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
    let build = tq_wire::build_info!("tq-edge");
    let version: &'static str = Box::leak(build.long().into_boxed_str());
    let args = Args::from_arg_matches(&Args::command().version(version).get_matches()).unwrap_or_else(|e| e.exit());
    let mut cfg = config::Config::load(&args.config)?;
    if let Some(url) = args.relay_url {
        cfg.relay.url = url;
    }
    tracing::info!(
        %build, room = %cfg.room, broadcast = %cfg.broadcast, relay = %cfg.relay.url,
        tracks = cfg.tracks.len(), channels = cfg.channels.len(), teleop = cfg.teleop.is_some(),
        "starting"
    );

    let (publication, inbox) = tq::Publication::new(cfg.advertisement())?;

    let mut stats = Vec::new();
    for t in &cfg.tracks {
        let s = Arc::new(source::Stats::default());
        source::spawn(t, publication.clone(), s.clone())?;
        stats.push((t.id.clone(), s));
    }

    let sinks: HashMap<String, sink::Sink> = cfg
        .channels
        .iter()
        .map(|c| (c.id.clone(), sink::Sink::start(&c.id, &c.sink, None)))
        .collect();
    let teleop = match &cfg.teleop {
        Some(t) => {
            // A motion program that (re)connects starts stopped.
            let hello = serde_json::json!({ "type": "safe_stop", "reason": "sink connected" });
            let motion = sink::TeleopSink(sink::Sink::start("teleop", &t.sink, Some(hello)));
            Some(tq_teleop::Teleop::new(&publication, Arc::new(motion))?)
        }
        None => None,
    };

    tokio::spawn(uplink::route(inbox, teleop.clone(), sinks));
    tokio::spawn(uplink::status(publication.clone(), teleop, stats, cfg.status_interval_ms));
    tokio::spawn(uplink::keyframe_requests(publication.clone()));

    tokio::select! {
        _ = uplink::run(Arc::new(cfg), build, publication) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
