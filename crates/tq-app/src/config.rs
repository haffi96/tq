//! Settings file, for the app started from Finder or a desktop launcher (no
//! flags, no shell environment). Command-line flags override it.
//!
//! `~/Library/Application Support/tq-app/config.json` on macOS,
//! `$XDG_CONFIG_HOME/tq-app/config.json` (default `~/.config`) on Linux:
//!
//! ```json
//! {
//!   "relay_http": "https://tq-relay-1.haffi.dev",
//!   "room": "robot-01",
//!   "role": "operator",
//!   "secret": "…"
//! }
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use tq_proto::Role;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub relay_http: Option<String>,
    pub relay: Option<String>,
    pub room: Option<String>,
    pub role: Option<Role>,
    pub token: Option<String>,
    pub secret: Option<String>,
    #[serde(default)]
    pub update: Update,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    /// GitHub `owner/name` the releases come from.
    pub repo: Option<String>,
    /// GitHub API base (tests point it at a mock).
    pub api: Option<String>,
    /// GitHub token: optional (higher API rate limit, or a private fork).
    pub token: Option<String>,
    /// Offer pre-releases too.
    #[serde(default)]
    pub prerelease: bool,
}

#[cfg(target_os = "macos")]
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Application Support/tq-app/config.json"))
}

#[cfg(not(target_os = "macos"))]
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(&std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("tq-app/config.json"))
}

impl File {
    /// `path`, or the default location; a missing default file is fine.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let (path, required) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(Self::default()),
            Err(e) => Err(anyhow::Error::from(e).context(format!("{}", path.display()))),
        }
    }
}
