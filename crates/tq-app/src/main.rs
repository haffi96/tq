//! tq-app: native operator / viewer for the tq relay, replacing the web
//! page. GPUI draws the window; the tq protocol runs in-process on tokio.
//! On macOS VideoToolbox decodes in hardware into CVPixelBuffers that GPUI
//! composites without a copy; on Linux NVDEC decodes and the frames are
//! drawn as images.
//!
//! ```text
//! tq-app --role operator --av robot-01      # relay on http://127.0.0.1:8080
//! ```
//!
//! Started without flags (from Finder, or a desktop launcher) it reads its
//! settings file ([`config`]); flags override the file.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("tq-app decodes with VideoToolbox (macOS) or NVDEC (Linux) and only builds there");

mod config;
mod decode;
mod feed;
#[cfg(target_os = "linux")]
mod nvdec;
mod session;
mod stats;
mod ui;
mod update;
#[cfg(target_os = "macos")]
mod vt;

use std::path::PathBuf;

use clap::{CommandFactory, FromArgMatches, Parser};
use gpui_kit::{
    actions,
    component::{Theme, TitleBar},
    px, size, App, AppContext, Bounds, KeyBinding, WindowBounds, WindowOptions,
};
use tq_proto::Role;

actions!(tq_app, [Quit]);

#[derive(Parser)]
#[command(about = "Native tq operator / viewer")]
struct Args {
    /// Settings file; default ~/Library/Application Support/tq-app/config.json
    /// on macOS, ~/.config/tq-app/config.json on Linux.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Relay HTTP endpoint (certificate hash, QUIC port, dev tokens).
    /// Default http://127.0.0.1:8080.
    #[arg(long)]
    relay_http: Option<url::Url>,
    /// Relay QUIC `host:port`; default: the HTTP host and the relay's QUIC port.
    #[arg(long)]
    relay: Option<String>,
    /// Default robot-01.
    #[arg(long)]
    av: Option<String>,
    /// operator or viewer (default).
    #[arg(long)]
    role: Option<Role>,
    /// Session token. Without one the app signs its own with --secret, or
    /// asks a --dev relay for one.
    #[arg(long)]
    token: Option<String>,
    #[arg(long, env = "TQ_SHARED_SECRET", hide_env_values = true)]
    secret: Option<String>,
    /// Also log each camera's numbers every 2 s (headless checks).
    #[arg(long)]
    log_stats: bool,
    /// Print whether a newer release exists, then exit.
    #[arg(long)]
    check_update: bool,
    /// Install the newest release over this one, then exit (no restart).
    #[arg(long)]
    install_update: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();
    let build = tq_proto::build_info!("tq-app");
    let version: &'static str = Box::leak(build.long().into_boxed_str());
    let args = Args::from_arg_matches(&Args::command().version(version).get_matches()).unwrap_or_else(|e| e.exit());
    let file = config::File::load(args.config.as_deref())?;
    let updates = update::Settings::resolve(&file.update);

    if args.check_update || args.install_update {
        return update_cli(&updates, args.install_update);
    }
    update::clean_stale_staging();

    let role = args.role.or(file.role).unwrap_or(Role::Viewer);
    if role == Role::Publisher {
        anyhow::bail!("--role must be operator or viewer");
    }
    tracing::info!(%build, "starting");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("tq-net")
        .enable_all()
        .build()?;
    let (events_tx, events_rx) = futures::channel::mpsc::unbounded();
    let http = match (args.relay_http, file.relay_http) {
        (Some(u), _) => u,
        (None, Some(s)) => s.parse().map_err(|e| anyhow::anyhow!("config relay_http {s:?}: {e}"))?,
        (None, None) => "http://127.0.0.1:8080".parse()?,
    };
    let config = session::Config {
        http,
        relay: args.relay.or(file.relay),
        av: args.av.or(file.av).unwrap_or_else(|| "robot-01".into()),
        role,
        token: args.token.or(file.token),
        secret: args.secret.or(file.secret),
        log_stats: args.log_stats,
        build,
    };
    let title = format!("tq · {} · {}", config.av, config.role);
    let shared = session::start(&rt, config, events_tx);

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..TitleBar::window_options()
            },
            cx,
            |window, cx| {
                window.set_window_title(&title);
                // Follow the system appearance; the title bar toggles it.
                Theme::sync_system_appearance(Some(window), cx);
                window
                    .observe_window_appearance(|window, cx| Theme::sync_system_appearance(Some(window), cx))
                    .detach();
                cx.new(|cx| ui::AppView::new(shared, events_rx, updates, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    drop(rt);
    Ok(())
}

/// `--check-update` / `--install-update`.
fn update_cli(updates: &update::Settings, install: bool) -> anyhow::Result<()> {
    let current = update::current();
    let Some(release) = update::check(updates)? else {
        println!("tq-app {current} is up to date ({})", updates.repo);
        return Ok(());
    };
    println!(
        "tq-app {} is available (this is {current}): {}",
        release.version, release.page
    );
    if install {
        let installed = update::install(updates, &release, &|s| println!("{s}…"))?;
        println!(
            "installed {} at {}; start it again to use it",
            release.version,
            installed.display()
        );
    }
    Ok(())
}
