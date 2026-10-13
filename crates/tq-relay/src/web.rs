//! HTTP side of the relay: the browser app, the certificate hash for
//! `serverCertificateHashes`, and (with --dev) token minting.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use tower_http::services::{ServeDir, ServeFile};
use tq_proto::{now_us, sign_token, Claims, Role};

use crate::Relay;

pub async fn serve(relay: Arc<Relay>, addr: SocketAddr, web_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let mut app = Router::new()
        .route("/api/cert", get(cert))
        .route("/api/info", get(info))
        .route("/api/avs", get(avs))
        .route("/api/dev/token", get(dev_token))
        .with_state(relay);
    if let Some(dir) = web_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "HTTP listening");
    axum::serve(listener, app).await?;
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

async fn avs(State(relay): State<Arc<Relay>>) -> impl IntoResponse {
    let avs: Vec<_> = relay.avs.lock().unwrap().values().cloned().collect();
    Json(avs.iter().map(|a| a.state()).collect::<Vec<_>>())
}

#[derive(Deserialize)]
struct TokenQuery {
    role: Role,
    av: String,
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
        av: q.av,
        exp: now_us() / 1_000_000 + 12 * 3600,
    };
    Json(serde_json::json!({ "token": sign_token(relay.secret.as_bytes(), &claims) })).into_response()
}
