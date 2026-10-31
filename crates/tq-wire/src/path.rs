//! Names and paths. Broadcasts, tracks, channels and locks have names;
//! anything inside a broadcast is addressed as `<broadcast>/<name>`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Longest name accepted.
pub const MAX_NAME_LEN: usize = 64;

/// Whether `s` is a valid name: 1–64 of `[A-Za-z0-9_.-]`. Names starting
/// with `.` are SDK conventions (`.status`, `.events`).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_NAME_LEN
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `<broadcast>/<name>`: a track, channel or lock.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Path {
    pub broadcast: String,
    pub name: String,
}

impl Path {
    pub fn new(broadcast: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            broadcast: broadcast.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.broadcast, self.name)
    }
}

impl std::str::FromStr for Path {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((b, n)) if valid_name(b) && valid_name(n) => Ok(Path::new(b, n)),
            _ => Err(format!("'{s}' is not a <broadcast>/<name> path")),
        }
    }
}

impl Serialize for Path {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Path {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        let p: Path = "rover-7/front".parse().unwrap();
        assert_eq!(p, Path::new("rover-7", "front"));
        assert_eq!(p.to_string(), "rover-7/front");
        assert!("rover-7/.status".parse::<Path>().is_ok());
        for bad in ["", "a", "a/", "/b", "a/b/c", "a b/c", "a/*"] {
            assert!(bad.parse::<Path>().is_err(), "{bad}");
        }
        assert_eq!(serde_json::to_string(&p).unwrap(), r#""rover-7/front""#);
    }
}
