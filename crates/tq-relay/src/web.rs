//! HTTP side of the relay: the browser app, the certificate hash for
//! `serverCertificateHashes`, and (with --dev) token minting.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{
    extract::{Query, State},
    http::{HeaderValue, Method, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use axum_server::tls_rustls::RustlsConfig;
use serde::Deserialize;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::{ServeDir, ServeFile},
};
use tq_proto::{now_us, sign_token, Claims, Role};
use tq_transport::cert::Certificate;

use crate::Relay;

pub struct Config {
    pub addr: SocketAddr,
    pub web_dir: Option<PathBuf>,
    /// Serve over TLS (same certificate as WebTransport).
    pub tls: Option<RustlsConfig>,
    pub cors_origins: Vec<String>,
}

/// TLS for the HTTP listener, from the WebTransport certificate.
pub fn tls_config(cert: &Certificate) -> anyhow::Result<RustlsConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(cert.chain.clone(), cert.clone_key())
        .context("HTTP TLS certificate")?;
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(tls)))
}

pub async fn serve(relay: Arc<Relay>, cfg: Config) -> anyhow::Result<()> {
    let mut app = Router::new()
        .route("/api/cert", get(cert))
        .route("/api/info", get(info))
        .route("/api/rooms", get(rooms))
        .route("/api/dev/token", get(dev_token))
        .with_state(relay);
    if !cfg.cors_origins.is_empty() {
        let origins = cfg
            .cors_origins
            .iter()
            .map(|o| HeaderValue::from_str(o).with_context(|| format!("CORS origin {o:?}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        app = app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET]),
        );
    }
    if let Some(dir) = cfg.web_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)));
    }
    let addr = cfg.addr;
    match cfg.tls {
        Some(tls) => {
            tracing::info!(%addr, "HTTPS listening");
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tracing::info!(%addr, "HTTP listening");
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

async fn cert(State(relay): State<Arc<Relay>>) -> impl IntoResponse {
    Json(serde_json::json!({ "sha256": relay.cert_hash, "self_signed": relay.self_signed }))
}

async fn info(State(relay): State<Arc<Relay>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "quic_port": relay.quic_port,
        "sha256": relay.cert_hash,
        "self_signed": relay.self_signed,
        "dev": relay.dev,
        "relay_us": now_us(),
        // Clients send `hello` only to relays that report a build.
        "build": relay.build,
    }))
}

async fn rooms(State(relay): State<Arc<Relay>>) -> impl IntoResponse {
    let rooms: Vec<_> = relay.rooms.lock().unwrap().values().cloned().collect();
    Json(rooms.iter().map(|r| r.state()).collect::<Vec<_>>())
}

#[derive(Deserialize)]
struct TokenQuery {
    role: Role,
    room: String,
    #[serde(default)]
    sub: Option<String>,
}

/// Mints a browser token. Only with `--dev`: anyone who can reach the HTTP
/// port can drive.
async fn dev_token(State(relay): State<Arc<Relay>>, Query(q): Query<TokenQuery>) -> axum::response::Response {
    if !relay.dev {
        return (StatusCode::NOT_FOUND, "token minting is only enabled with --dev").into_response();
    }
    if q.role == Role::Publisher {
        return (StatusCode::FORBIDDEN, "publisher tokens are not minted over HTTP").into_response();
    }
    let claims = Claims {
        sub: q.sub.unwrap_or_else(|| format!("dev-{}", q.role)),
        role: q.role,
        room: q.room,
        exp: now_us() / 1_000_000 + 12 * 3600,
    };
    Json(serde_json::json!({ "token": sign_token(relay.secret.as_bytes(), &claims) })).into_response()
}
