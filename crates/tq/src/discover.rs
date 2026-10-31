//! Finding a relay over HTTP(S): its QUIC port, how to trust it, its
//! build, and (from a dev relay) a token. Turns a relay's web address into
//! [`Options`] for [`Session::connect`](crate::Session::connect).

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tq_transport::quic::ServerTrust;
use tq_wire::{now_us, BuildInfo, Claims, Compat, Perms};

use crate::Options;

/// What a relay says about itself at `/api/info`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RelayInfo {
    pub quic_port: u16,
    /// Hex SHA-256 of its certificate.
    pub sha256: String,
    /// Whether that certificate is self-signed (trust it by hash) or from a
    /// CA (trust the system roots).
    pub self_signed: bool,
    #[serde(default)]
    pub build: Option<BuildInfo>,
}

impl RelayInfo {
    pub fn trust(&self) -> anyhow::Result<ServerTrust> {
        Ok(if self.self_signed {
            ServerTrust::Hashes(vec![tq_transport::cert::parse_hash(&self.sha256)?])
        } else {
            ServerTrust::SystemRoots
        })
    }
}

/// The relay speaks another protocol major. Retrying soon will not help.
#[derive(Debug)]
pub struct Incompatible(pub String);

impl std::fmt::Display for Incompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "incompatible relay: {}", self.0)
    }
}

impl std::error::Error for Incompatible {}

/// Where a session's token comes from.
#[derive(Debug, Clone)]
pub enum Token {
    Given(String),
    /// Signed here with the relay's shared secret.
    Signed {
        secret: String,
        sub: String,
        perms: Perms,
        ttl: Duration,
    },
    /// Minted by a `--dev` relay (`/api/dev/token`) from a preset:
    /// `operator` or `viewer`.
    Dev { preset: String, sub: String },
}

impl Token {
    /// Signed here with the relay's shared secret, allowed to publish
    /// broadcast `name` (the robot preset). The relay never mints these
    /// over HTTP.
    pub fn publisher(secret: impl Into<String>, name: &str) -> Self {
        Token::Signed {
            secret: secret.into(),
            sub: name.to_string(),
            perms: Perms::robot(name),
            ttl: Duration::from_secs(24 * 3600),
        }
    }

    async fn get(&self, http: &url::Url, room: &str) -> anyhow::Result<String> {
        Ok(match self {
            Token::Given(t) => t.clone(),
            Token::Signed { secret, sub, perms, ttl } => tq_wire::sign_token(
                secret.as_bytes(),
                &Claims {
                    sub: sub.clone(),
                    room: room.to_string(),
                    exp: now_us() / 1_000_000 + ttl.as_secs(),
                    perms: perms.clone(),
                },
            ),
            Token::Dev { preset, sub } => dev_token(http, room, preset, sub)
                .await
                .context("no token given and the relay does not mint dev tokens")?,
        })
    }
}

/// `GET /api/info`.
pub async fn relay_info(http: &url::Url) -> anyhow::Result<RelayInfo> {
    get_json(&http.join("/api/info")?).await.context("relay /api/info")
}

/// A token from a `--dev` relay, for a preset (`operator` or `viewer`).
pub async fn dev_token(http: &url::Url, room: &str, preset: &str, sub: &str) -> anyhow::Result<String> {
    let mut url = http.join("/api/dev/token")?;
    url.query_pairs_mut()
        .append_pair("preset", preset)
        .append_pair("room", room)
        .append_pair("sub", sub);
    let v: serde_json::Value = get_json(&url).await?;
    Ok(v["token"].as_str().context("no token in the response")?.to_string())
}

impl Options {
    /// Options for the relay whose web address is `http`: its QUIC
    /// endpoint (on `quic_host`, else the same host), its trust, and a
    /// token. Fails with [`Incompatible`] for a relay of another protocol
    /// major.
    pub async fn discover(
        http: &url::Url,
        quic_host: Option<&str>,
        room: &str,
        token: &Token,
        build: BuildInfo,
    ) -> anyhow::Result<Options> {
        let info = relay_info(http).await?;
        if Compat::of_build(info.build.as_ref()) == Compat::Incompatible {
            return Err(Incompatible(tq_wire::mismatch("relay", info.build.as_ref(), &build.component)).into());
        }
        let host = match quic_host {
            Some(h) => h.to_string(),
            None => {
                let h = http.host_str().context("relay URL has no host")?;
                // url keeps IPv6 brackets in host_str; add them if missing.
                let h = if h.contains(':') && !h.starts_with('[') {
                    format!("[{h}]")
                } else {
                    h.to_string()
                };
                format!("{h}:{}", info.quic_port)
            }
        };
        Ok(Options {
            relay_url: url::Url::parse(&format!("https://{host}"))?,
            room: room.to_string(),
            token: token.get(http, room).await?,
            trust: info.trust()?,
            quic: Default::default(),
            build,
        })
    }
}

/// `GET` a JSON document over HTTP/1.0: plain for `http://` (dev and
/// self-signed relays), TLS over the system roots for `https://`.
pub async fn get_json<T: serde::de::DeserializeOwned>(url: &url::Url) -> anyhow::Result<T> {
    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, request: &str) -> anyhow::Result<Vec<u8>> {
        s.write_all(request.as_bytes()).await?;
        // A TLS stream may still hold the encrypted request until flushed.
        s.flush().await?;
        let mut body = Vec::new();
        match s.read_to_end(&mut body).await {
            Ok(_) => {}
            // Some servers close without a TLS close_notify; the HTTP/1.0
            // body ends at the close either way.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !body.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(body)
    }

    let host = url.host_str().context("no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    // 5 s for the whole request, of which at most 3 s to connect.
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let tcp = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(&addr))
            .await
            .context("connect timed out")??;
        match url.scheme() {
            "https" => {
                let tls = tokio_rustls::TlsConnector::from(Arc::new(tq_transport::quic::https_client_config()?));
                let name = host.trim_start_matches('[').trim_end_matches(']').to_string();
                let name = tokio_rustls::rustls::pki_types::ServerName::try_from(name).context("bad TLS server name")?;
                exchange(tls.connect(name, tcp).await?, &request).await
            }
            "http" => exchange(tcp, &request).await,
            other => anyhow::bail!("unsupported scheme {other}://"),
        }
    })
    .await
    .with_context(|| format!("{url}: timed out"))??;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").context("bad HTTP response")?;
    let status = head.split_whitespace().nth(1).unwrap_or("");
    anyhow::ensure!(status == "200", "{url}: HTTP {status}: {}", body.trim());
    serde_json::from_str(body).with_context(|| format!("{url}: response is not JSON"))
}
