//! Session tokens: compact HS256 JWTs signed with the relay's shared
//! secret. Passed as `?token=` on the WebTransport URL.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::Role;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Who this is (shown in the UI and logs).
    pub sub: String,
    pub role: Role,
    /// Robot (av id) the token is valid for, or `*`.
    pub av: String,
    /// Expiry, Unix seconds.
    pub exp: u64,
}

impl Claims {
    pub fn allows_av(&self, av_id: &str) -> bool {
        self.av == "*" || self.av == av_id
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
    #[error("bad signature")]
    Signature,
    #[error("token expired")]
    Expired,
}

const HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

fn mac(secret: &[u8]) -> Hmac<Sha256> {
    Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length")
}

pub fn sign_token(secret: &[u8], claims: &Claims) -> String {
    let header = B64.encode(HEADER);
    let payload = B64.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let signing_input = format!("{header}.{payload}");
    let mut m = mac(secret);
    m.update(signing_input.as_bytes());
    let sig = B64.encode(m.finalize().into_bytes());
    format!("{signing_input}.{sig}")
}

pub fn verify_token(secret: &[u8], token: &str, now_secs: u64) -> Result<Claims, TokenError> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };
    let sig = B64.decode(sig).map_err(|_| TokenError::Malformed)?;
    let mut m = mac(secret);
    m.update(header.as_bytes());
    m.update(b".");
    m.update(payload.as_bytes());
    m.verify_slice(&sig).map_err(|_| TokenError::Signature)?;
    let header: serde_json::Value = serde_json::from_slice(&B64.decode(header).map_err(|_| TokenError::Malformed)?)
        .map_err(|_| TokenError::Malformed)?;
    if header.get("alg").and_then(|v| v.as_str()) != Some("HS256") {
        return Err(TokenError::Malformed);
    }
    let claims: Claims = serde_json::from_slice(&B64.decode(payload).map_err(|_| TokenError::Malformed)?)
        .map_err(|_| TokenError::Malformed)?;
    if claims.exp <= now_secs {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let c = Claims {
            sub: "op-1".into(),
            role: Role::Operator,
            av: "robot-01".into(),
            exp: 2000,
        };
        let t = sign_token(b"secret", &c);
        assert_eq!(verify_token(b"secret", &t, 1000).unwrap(), c);
        assert_eq!(verify_token(b"other", &t, 1000).unwrap_err(), TokenError::Signature);
        assert_eq!(verify_token(b"secret", &t, 2000).unwrap_err(), TokenError::Expired);
        assert_eq!(verify_token(b"secret", "a.b", 0).unwrap_err(), TokenError::Malformed);
        assert!(c.allows_av("robot-01") && !c.allows_av("robot-02"));
    }
}
