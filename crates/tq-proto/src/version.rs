//! Versions: the wire protocol's, and each component's build.
//!
//! The protocol has a major and a minor number. A minor bump adds fields
//! or messages that older peers ignore; a major bump changes something a
//! peer cannot ignore. Peers with different majors refuse each other
//! ([`crate::code::UNSUPPORTED_VERSION`]); a different minor only warns.
//!
//! Every component sends its [`BuildInfo`] in its first message (publisher
//! `Announce`, client `Hello`; the relay in `Welcome`, `AnnounceOk` and
//! `/api/info`). Those fields, `Hello` and the close code must keep their
//! shape across majors, so that peers can still tell each other apart.

use serde::{Deserialize, Serialize};

/// The protocol this build speaks.
pub const PROTOCOL: ProtocolVersion = ProtocolVersion { major: 1, minor: 1 };

/// What a peer without build info speaks: everything before 1.1.
pub const PROTOCOL_LEGACY: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Who a peer is: sent in the first message of each session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// `tq-relay`, `tq-publisher`, `tq-app`, `tq-web` or `tq-probe`.
    pub component: String,
    /// The component's own semver.
    pub version: String,
    /// Short commit, `-dirty` when built from a modified tree.
    #[serde(default)]
    pub git_sha: String,
    pub protocol: ProtocolVersion,
}

impl BuildInfo {
    /// `0.3.1 (a1b2c3d4e5)`.
    pub fn short(&self) -> String {
        if self.git_sha.is_empty() || self.git_sha == "unknown" {
            self.version.clone()
        } else {
            format!("{} ({})", self.version, self.git_sha)
        }
    }

    /// For `--version`: `0.3.1 (a1b2c3d4e5) protocol 1.1`.
    pub fn long(&self) -> String {
        format!("{} protocol {}", self.short(), self.protocol)
    }
}

impl std::fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} protocol {}", self.component, self.short(), self.protocol)
    }
}

/// This build's [`BuildInfo`]. Expands in the calling crate, so the version
/// is that crate's; the commit comes from its build script
/// (`build/version.rs` sets `TQ_GIT_SHA`).
#[macro_export]
macro_rules! build_info {
    ($component:expr) => {
        $crate::BuildInfo {
            component: ($component).to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            git_sha: option_env!("TQ_GIT_SHA").unwrap_or("unknown").to_string(),
            protocol: $crate::PROTOCOL,
        }
    };
}

/// How a peer's protocol relates to ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    Same,
    /// Works; one side may not know some fields or messages.
    MinorDiffers,
    /// Different major: the session must not go on.
    Incompatible,
}

impl Compat {
    pub fn of(peer: Option<ProtocolVersion>) -> Self {
        let peer = peer.unwrap_or(PROTOCOL_LEGACY);
        if peer.major != PROTOCOL.major {
            Compat::Incompatible
        } else if peer.minor != PROTOCOL.minor {
            Compat::MinorDiffers
        } else {
            Compat::Same
        }
    }

    /// Checks a peer's build info; `None` means a pre-1.1 peer.
    pub fn of_build(peer: Option<&BuildInfo>) -> Self {
        Self::of(peer.map(|b| b.protocol))
    }
}

/// One line for logs, the UI and close reasons, readable on either side:
/// `relay 0.3.1 (a1b2c3d) speaks protocol 2.0, tq-app speaks 1.1`.
pub fn mismatch(peer_name: &str, peer: Option<&BuildInfo>, me: &str) -> String {
    match peer {
        Some(b) => format!(
            "{peer_name} {} speaks protocol {}, {me} speaks {PROTOCOL}",
            b.short(),
            b.protocol
        ),
        None => format!("{peer_name} predates version reporting (protocol {PROTOCOL_LEGACY}), {me} speaks {PROTOCOL}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(major: u16, minor: u16) -> BuildInfo {
        BuildInfo {
            component: "tq-relay".into(),
            version: "1.2.3".into(),
            git_sha: "abc".into(),
            protocol: ProtocolVersion { major, minor },
        }
    }

    #[test]
    fn compat() {
        assert_eq!(
            Compat::of_build(Some(&build(PROTOCOL.major, PROTOCOL.minor))),
            Compat::Same
        );
        assert_eq!(
            Compat::of_build(Some(&build(PROTOCOL.major, PROTOCOL.minor + 1))),
            Compat::MinorDiffers
        );
        assert_eq!(
            Compat::of_build(Some(&build(PROTOCOL.major + 1, 0))),
            Compat::Incompatible
        );
        // Peers from before version reporting speak 1.0.
        assert_eq!(Compat::of_build(None), Compat::of(Some(PROTOCOL_LEGACY)));
    }

    #[test]
    fn macro_uses_caller_version() {
        let b = crate::build_info!("tq-proto-test");
        assert_eq!(b.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(b.protocol, PROTOCOL);
        assert_eq!(b.long(), format!("{} protocol {PROTOCOL}", b.short()));
    }
}
