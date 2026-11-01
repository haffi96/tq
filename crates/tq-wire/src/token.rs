//! Session tokens: compact HS256 JWTs signed with the relay's shared
//! secret, passed as `?token=` on the WebTransport URL. They carry
//! permissions ([`Perms`]) rather than a role.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{valid_name, Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Who this is (shown in the UI and logs).
    pub sub: String,
    /// Room the token is valid for, or `*`.
    pub room: String,
    /// Expiry, Unix seconds.
    pub exp: u64,
    pub perms: Perms,
}

impl Claims {
    pub fn allows_room(&self, room_id: &str) -> bool {
        self.room == "*" || self.room == room_id
    }
}

/// What a permission allows; see [`Perms`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    /// Advertise a broadcast with a matching name.
    Pub,
    /// Subscribe to a matching track.
    Sub,
    /// Send on a matching channel.
    Write,
    /// Claim a matching lock while it is free.
    Lock,
    /// Take a matching lock from its holder.
    Force,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Verb::Pub => "pub",
            Verb::Sub => "sub",
            Verb::Write => "write",
            Verb::Lock => "lock",
            Verb::Force => "force",
        }
    }
}

/// A token's permissions: `verb:pattern` strings, such as `pub:rover-7`,
/// `sub:rover-7/*` or `write:*/drive`. A pattern segment may be `*`. `pub`
/// takes a broadcast name; the other verbs a `<broadcast>/<name>` path.
/// Strings that do not parse are kept (and shown) but allow nothing, so a
/// token from a newer minor with new verbs still works for the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Perms(pub Vec<String>);

impl Perms {
    pub fn new<I: IntoIterator<Item = S>, S: Into<String>>(perms: I) -> Self {
        Self(perms.into_iter().map(Into::into).collect())
    }

    /// Everything: subscribe, write, lock and force anywhere.
    pub fn operator() -> Self {
        Self::new(["sub:*/*", "write:*/*", "lock:*/*", "force:*/*"])
    }

    /// Subscribe to everything.
    pub fn viewer() -> Self {
        Self::new(["sub:*/*"])
    }

    /// Publish `broadcast` (any name for `*`).
    pub fn robot(broadcast: &str) -> Self {
        Self::new([format!("pub:{broadcast}")])
    }

    /// Named presets for dev tokens and the CLI: `operator`, `viewer`, or
    /// `robot` / `robot:<broadcast>` (default `*`).
    pub fn preset(name: &str) -> Option<Self> {
        match name.split_once(':') {
            None if name == "operator" => Some(Self::operator()),
            None if name == "viewer" => Some(Self::viewer()),
            None if name == "robot" => Some(Self::robot("*")),
            Some(("robot", b)) if b == "*" || valid_name(b) => Some(Self::robot(b)),
            _ => None,
        }
    }

    pub fn allows_pub(&self, broadcast: &str) -> bool {
        self.0.iter().any(|p| {
            p.split_once(':')
                .is_some_and(|(v, pat)| v == Verb::Pub.as_str() && segment(pat, broadcast))
        })
    }

    pub fn allows(&self, verb: Verb, path: &Path) -> bool {
        if verb == Verb::Pub {
            return false;
        }
        self.0.iter().any(|p| {
            let Some((v, pat)) = p.split_once(':') else {
                return false;
            };
            let Some((b, n)) = pat.split_once('/') else {
                return false;
            };
            v == verb.as_str() && segment(b, &path.broadcast) && segment(n, &path.name)
        })
    }

    /// Whether any `pub` permission exists at all.
    pub fn can_publish(&self) -> bool {
        self.0.iter().any(|p| p.starts_with("pub:"))
    }
}

fn segment(pattern: &str, value: &str) -> bool {
    pattern == "*" || pattern == value
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
            room: "robot-01".into(),
            exp: 2000,
            perms: Perms::operator(),
        };
        let t = sign_token(b"secret", &c);
        assert_eq!(verify_token(b"secret", &t, 1000).unwrap(), c);
        assert_eq!(verify_token(b"other", &t, 1000).unwrap_err(), TokenError::Signature);
        assert_eq!(verify_token(b"secret", &t, 2000).unwrap_err(), TokenError::Expired);
        assert_eq!(verify_token(b"secret", "a.b", 0).unwrap_err(), TokenError::Malformed);
        assert!(c.allows_room("robot-01") && !c.allows_room("robot-02"));
    }

    #[test]
    fn patterns() {
        let p = Perms::new([
            "sub:rover-7/*",
            "sub:*/.status",
            "write:rover-7/drive",
            "pub:alice",
            "future:thing",
            "garbage",
        ]);
        let path = |s: &str| s.parse::<Path>().unwrap();
        assert!(p.allows(Verb::Sub, &path("rover-7/front")));
        assert!(p.allows(Verb::Sub, &path("other/.status")));
        assert!(!p.allows(Verb::Sub, &path("other/front")));
        assert!(p.allows(Verb::Write, &path("rover-7/drive")));
        assert!(!p.allows(Verb::Write, &path("rover-7/stop")));
        assert!(!p.allows(Verb::Lock, &path("rover-7/drive")));
        assert!(p.allows_pub("alice") && !p.allows_pub("bob") && p.can_publish());
        assert!(Perms::robot("*").allows_pub("anything"));
        assert!(!Perms::viewer().can_publish());
        assert_eq!(Perms::preset("robot:rover-7"), Some(Perms::robot("rover-7")));
        assert_eq!(Perms::preset("admin"), None);
    }
}
