//! TLS certificates for the relay.
//!
//! Browsers accept a self-signed certificate over WebTransport only through
//! `serverCertificateHashes`, and only if it is ECDSA and valid for at most
//! 14 days. [`self_signed`] makes one valid for 10 days; restart the relay
//! (or pass a real certificate with `--cert/--key`) before it runs out.

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub struct Certificate {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// SHA-256 of the leaf certificate (DER), for `serverCertificateHashes`.
    pub sha256: [u8; 32],
    pub self_signed: bool,
}

impl Certificate {
    pub fn hash_hex(&self) -> String {
        hex::encode(self.sha256)
    }

    pub fn clone_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

pub fn self_signed(names: &[String]) -> anyhow::Result<Certificate> {
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(10);
    params.distinguished_name.push(rcgen::DnType::CommonName, "tq-relay");
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let cert = params.self_signed(&key)?;
    let der = CertificateDer::from(cert.der().to_vec());
    let sha256 = Sha256::digest(der.as_ref()).into();
    Ok(Certificate {
        chain: vec![der],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        sha256,
        self_signed: true,
    })
}

pub fn load_pem(cert: &std::path::Path, key: &std::path::Path) -> anyhow::Result<Certificate> {
    let pem = std::fs::read(cert).with_context(|| format!("read {}", cert.display()))?;
    let chain: Vec<_> = rustls_pemfile::certs(&mut pem.as_slice()).collect::<Result<_, _>>()?;
    anyhow::ensure!(!chain.is_empty(), "no certificate in {}", cert.display());
    let pem = std::fs::read(key).with_context(|| format!("read {}", key.display()))?;
    let key = rustls_pemfile::private_key(&mut pem.as_slice())?.context("no private key")?;
    let sha256 = Sha256::digest(chain[0].as_ref()).into();
    Ok(Certificate {
        chain,
        key,
        sha256,
        self_signed: false,
    })
}

/// Parses a hex SHA-256 (colons allowed).
pub fn parse_hash(s: &str) -> anyhow::Result<[u8; 32]> {
    let clean: String = s.chars().filter(|c| *c != ':').collect();
    let bytes = hex::decode(clean.trim()).context("certificate hash is not hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("certificate hash must be 32 bytes"))
}
