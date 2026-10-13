//! Endpoint construction with the transport knobs that matter for latency.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Congestion {
    /// Model-based; does not treat random loss as congestion (Starlink).
    #[default]
    Bbr,
    Cubic,
    NewReno,
}

impl std::str::FromStr for Congestion {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bbr" => Ok(Congestion::Bbr),
            "cubic" => Ok(Congestion::Cubic),
            "new_reno" | "newreno" => Ok(Congestion::NewReno),
            other => Err(format!(
                "unknown congestion controller '{other}' (bbr, cubic, new_reno)"
            )),
        }
    }
}

/// Transport settings for one endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuicOptions {
    pub congestion: Congestion,
    /// Bytes the connection may have unacknowledged (queued + in flight)
    /// across all streams. Writes block beyond it, which is what lets a
    /// sender notice it is behind and drop to the next keyframe instead of
    /// queueing seconds of video inside QUIC.
    pub send_window: u64,
    pub idle_timeout_ms: u64,
    pub keep_alive_ms: u64,
    /// Initial RTT estimate before the first sample (quinn default 333 ms).
    pub initial_rtt_ms: u64,
    pub max_concurrent_uni_streams: u32,
}

impl Default for QuicOptions {
    fn default() -> Self {
        Self {
            congestion: Congestion::Bbr,
            send_window: 1 << 20,
            idle_timeout_ms: 5_000,
            keep_alive_ms: 1_000,
            initial_rtt_ms: 100,
            max_concurrent_uni_streams: 1024,
        }
    }
}

impl QuicOptions {
    pub fn transport_config(&self) -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        match self.congestion {
            Congestion::Bbr => t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default())),
            Congestion::Cubic => t.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default())),
            Congestion::NewReno => {
                t.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()))
            }
        };
        t.send_window(self.send_window);
        t.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(Duration::from_millis(self.idle_timeout_ms)).expect("idle timeout in range"),
        ));
        t.keep_alive_interval(Some(Duration::from_millis(self.keep_alive_ms)));
        t.initial_rtt(Duration::from_millis(self.initial_rtt_ms));
        t.max_concurrent_uni_streams(self.max_concurrent_uni_streams.into());
        t.datagram_receive_buffer_size(Some(1 << 20));
        t.datagram_send_buffer_size(64 << 10);
        Arc::new(t)
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A WebTransport server endpoint.
pub fn server(
    addr: SocketAddr,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    opts: &QuicOptions,
) -> anyhow::Result<web_transport_quinn::Server> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    let tls: quinn::crypto::rustls::QuicServerConfig = tls.try_into()?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(tls));
    config.transport_config(opts.transport_config());
    let endpoint = quinn::Endpoint::server(config, addr).with_context(|| format!("bind QUIC on {addr}"))?;
    Ok(web_transport_quinn::Server::new(endpoint))
}

/// How a client checks the server certificate.
#[derive(Debug, Clone)]
pub enum ServerTrust {
    /// The platform's root store (a real certificate, e.g. Let's Encrypt).
    SystemRoots,
    /// SHA-256 of the DER certificate (self-signed relay certificates).
    Hashes(Vec<[u8; 32]>),
    /// No verification at all. Development only.
    Insecure,
}

/// A WebTransport client endpoint.
pub fn client(trust: ServerTrust, opts: &QuicOptions) -> anyhow::Result<web_transport_quinn::Client> {
    let builder =
        rustls::ClientConfig::builder_with_provider(provider()).with_protocol_versions(&[&rustls::version::TLS13])?;
    let mut tls = match trust {
        ServerTrust::SystemRoots => {
            let mut roots = rustls::RootCertStore::empty();
            let native = rustls_native_certs();
            for cert in native {
                let _ = roots.add(cert);
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        ServerTrust::Hashes(hashes) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
                provider: provider(),
                hashes,
            }))
            .with_no_client_auth(),
        ServerTrust::Insecure => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
                provider: provider(),
                hashes: Vec::new(),
            }))
            .with_no_client_auth(),
    };
    tls.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];
    let tls: quinn::crypto::rustls::QuicClientConfig = tls.try_into()?;
    let mut config = quinn::ClientConfig::new(Arc::new(tls));
    config.transport_config(opts.transport_config());
    let endpoint = match quinn::Endpoint::client("[::]:0".parse()?) {
        Ok(e) => e,
        Err(_) => quinn::Endpoint::client("0.0.0.0:0".parse()?)?,
    };
    Ok(web_transport_quinn::Client::new(endpoint, config))
}

fn rustls_native_certs() -> Vec<CertificateDer<'static>> {
    // Avoid another dependency: read the system bundle when present.
    let candidates = [
        "/etc/ssl/cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
    ];
    for path in candidates {
        if let Ok(pem) = std::fs::read(path) {
            let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice())
                .filter_map(Result::ok)
                .collect();
            if !certs.is_empty() {
                return certs;
            }
        }
    }
    Vec::new()
}

/// Accepts a server certificate whose SHA-256 matches one of `hashes`; with
/// no hashes, accepts anything (insecure mode). Signatures are still
/// checked so the peer must hold the key.
#[derive(Debug)]
struct PinnedVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
    hashes: Vec<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.hashes.is_empty() {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        let hash: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if self.hashes.contains(&hash) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Path statistics of a connection, for the UI.
pub fn quic_stats(conn: &quinn::Connection) -> tq_proto::QuicStats {
    let s = conn.stats();
    tq_proto::QuicStats {
        rtt_ms: s.path.rtt.as_secs_f64() * 1000.0,
        cwnd: s.path.cwnd,
        lost_packets: s.path.lost_packets,
        sent_packets: s.path.sent_packets,
        congestion_events: s.path.congestion_events,
    }
}
