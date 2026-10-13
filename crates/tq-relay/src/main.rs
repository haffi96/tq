//! tq-relay: the meeting point between the vehicle and browsers.
//!
//! One WebTransport endpoint serves both sides:
//! - `/v1/publish/{av}`: the vehicle publisher (publisher token);
//! - `/v1/session/{av}`: browser operators and viewers (role in the token).
//!
//! Video is fanned out per camera from a GOP cache with a per-role drop
//! policy; teleop datagrams pass between the claim holder and the vehicle.

mod av;
mod client;
mod publisher;
mod web;

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, Mutex},
    time::Duration,
};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tq_proto::{now_us, sign_token, verify_token, BuildInfo, Claims, Role};
use tq_transport::{
    group::SendPolicy,
    quic::{Congestion, QuicOptions},
};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// WebTransport (UDP) listen address.
    #[arg(long, env = "TQ_QUIC", default_value = "[::]:4443")]
    quic: SocketAddr,
    /// HTTP listen address (browser app, /api/cert, /api/dev/token).
    #[arg(long, env = "TQ_HTTP", default_value = "[::]:8080")]
    http: SocketAddr,
    /// Shared secret for tokens.
    #[arg(long, env = "TQ_SHARED_SECRET", hide_env_values = true)]
    secret: String,
    /// PEM certificate chain. Without it a 10-day self-signed ECDSA
    /// certificate is generated at startup (browsers pin it by hash).
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,
    /// Names for the self-signed certificate.
    #[arg(long, value_delimiter = ',', default_value = "localhost,127.0.0.1,::1")]
    hostnames: Vec<String>,
    /// Directory with the built browser app (web/dist).
    #[arg(long, env = "TQ_WEB_DIR")]
    web_dir: Option<PathBuf>,
    /// Development mode: GET /api/dev/token mints browser tokens.
    #[arg(long)]
    dev: bool,
    /// Congestion controller for every connection.
    #[arg(long, default_value = "bbr")]
    congestion: Congestion,
    /// Connection send window (bytes queued + in flight) per session.
    #[arg(long, default_value_t = 1 << 20)]
    send_window: u64,
    /// Operator drop policy: frames older than this are dropped with the
    /// rest of their GOP.
    #[arg(long, default_value_t = 250)]
    operator_max_lag_ms: u64,
    /// Viewer drop policy.
    #[arg(long, default_value_t = 1500)]
    viewer_max_lag_ms: u64,
    /// Older GOPs a viewer may keep receiving after a new one starts.
    #[arg(long, default_value_t = 1)]
    viewer_keep_groups: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Print a token signed with --secret.
    Token {
        #[arg(long)]
        role: Role,
        #[arg(long)]
        av: String,
        #[arg(long, default_value = "cli")]
        sub: String,
        #[arg(long, default_value_t = 24)]
        ttl_hours: u64,
    },
}

pub struct Relay {
    pub build: BuildInfo,
    pub secret: String,
    pub avs: Mutex<HashMap<String, Arc<av::Av>>>,
    pub cert_hash: String,
    pub self_signed: bool,
    pub quic_port: u16,
    pub dev: bool,
    pub operator_policy: SendPolicy,
    pub viewer_policy: SendPolicy,
    pub next_conn: AtomicU64,
}

impl Relay {
    fn av(&self, id: &str) -> Arc<av::Av> {
        self.avs
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| av::Av::new(id.to_string()))
            .clone()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();
    let build = tq_proto::build_info!("tq-relay");
    let args = parse_args(&build);
    anyhow::ensure!(!args.secret.is_empty(), "--secret / TQ_SHARED_SECRET must not be empty");

    if let Some(Command::Token {
        role,
        av,
        sub,
        ttl_hours,
    }) = args.command
    {
        let claims = Claims {
            sub,
            role,
            av,
            exp: now_us() / 1_000_000 + ttl_hours * 3600,
        };
        println!("{}", sign_token(args.secret.as_bytes(), &claims));
        return Ok(());
    }

    let cert = match (&args.cert, &args.key) {
        (Some(c), Some(k)) => tq_transport::cert::load_pem(c, k)?,
        _ => tq_transport::cert::self_signed(&args.hostnames)?,
    };
    let quic = QuicOptions {
        congestion: args.congestion,
        send_window: args.send_window,
        ..Default::default()
    };
    let mut server = tq_transport::quic::server(args.quic, cert.chain.clone(), cert.clone_key(), &quic)?;
    tracing::info!(addr = %args.quic, congestion = ?args.congestion, sha256 = %cert.hash_hex(), self_signed = cert.self_signed, "WebTransport listening");

    tracing::info!(%build, "starting");
    let relay = Arc::new(Relay {
        build,
        secret: args.secret.clone(),
        avs: Mutex::new(HashMap::new()),
        cert_hash: cert.hash_hex(),
        self_signed: cert.self_signed,
        quic_port: args.quic.port(),
        dev: args.dev,
        operator_policy: SendPolicy {
            keep_groups: 0,
            max_lag: Duration::from_millis(args.operator_max_lag_ms),
        },
        viewer_policy: SendPolicy {
            keep_groups: args.viewer_keep_groups,
            max_lag: Duration::from_millis(args.viewer_max_lag_ms),
        },
        next_conn: AtomicU64::new(1),
    });

    if args.dev {
        let http = args.http;
        let port = http.port();
        tracing::warn!("--dev: GET /api/dev/token mints operator tokens for anyone who can reach port {port}");
        tracing::info!("operator UI: http://localhost:{port}/?av=robot-01&role=operator");
        tracing::info!("viewer UI:   http://localhost:{port}/?av=robot-01&role=viewer");
    }
    tokio::spawn(web::serve(relay.clone(), args.http, args.web_dir.clone()));

    loop {
        tokio::select! {
            req = server.accept() => {
                let Some(req) = req else { break };
                let relay = relay.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(relay, req).await {
                        tracing::info!(error = format!("{e:#}"), "session ended");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// `--version` with the commit and protocol: `0.1.0 (a1b2c3d4e5) protocol 1.1`.
fn parse_args(build: &BuildInfo) -> Args {
    let version: &'static str = Box::leak(build.long().into_boxed_str());
    let matches = Args::command().version(version).get_matches();
    Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

async fn handle(relay: Arc<Relay>, req: tq_transport::wt::Request) -> anyhow::Result<()> {
    let url = req.url.clone();
    let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
    let token = url
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v): (_, std::borrow::Cow<str>)| v.into_owned())
        .unwrap_or_default();
    let peer = req.conn().remote_address();

    let (kind, av_id) = match segments.as_slice() {
        ["v1", kind @ ("publish" | "session"), av] => (*kind, av.to_string()),
        _ => {
            req.reject(http::StatusCode::NOT_FOUND).await?;
            anyhow::bail!("{peer}: unknown path {}", url.path());
        }
    };
    let claims = match verify_token(relay.secret.as_bytes(), &token, now_us() / 1_000_000) {
        Ok(c) => c,
        Err(e) => {
            req.reject(http::StatusCode::UNAUTHORIZED).await?;
            anyhow::bail!("{peer}: {e}");
        }
    };
    let role_ok = match kind {
        "publish" => claims.role == Role::Publisher,
        _ => matches!(claims.role, Role::Operator | Role::Viewer),
    };
    if !role_ok || !claims.allows_av(&av_id) {
        req.reject(http::StatusCode::FORBIDDEN).await?;
        anyhow::bail!(
            "{peer}: token for {} {} not valid on {}",
            claims.role,
            claims.av,
            url.path()
        );
    }

    let session = req.ok().await?;
    let av = relay.av(&av_id);
    match kind {
        "publish" => publisher::run(relay, av, session, claims).await,
        _ => client::run(relay, av, session, claims).await,
    }
}
