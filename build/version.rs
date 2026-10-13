//! Build script shared by the binary crates (`build = "../../build/version.rs"`).
//! Sets `TQ_GIT_SHA` for `tq_proto::build_info!`: the short commit, with
//! `-dirty` when the tree had uncommitted changes. A `TQ_GIT_SHA` in the
//! environment wins (container builds have no `.git`).
//!
//! It reruns when HEAD moves or the index changes. Edits that are never
//! staged do not rerun it, so `-dirty` is best effort in local builds;
//! release builds come from a clean checkout of the tag.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=TQ_GIT_SHA");
    let sha = match std::env::var("TQ_GIT_SHA") {
        Ok(s) if !s.is_empty() => s,
        _ => match git(&["rev-parse", "--short=10", "HEAD"]) {
            Some(sha) => {
                for path in ["HEAD", "index"] {
                    if let Some(p) = git(&["rev-parse", "--git-path", path]) {
                        println!("cargo:rerun-if-changed={p}");
                    }
                }
                if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
                    if let Some(p) = git(&["rev-parse", "--git-path", &head]) {
                        println!("cargo:rerun-if-changed={p}");
                    }
                }
                // Tracked changes anywhere in the repository count.
                let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
                if dirty {
                    format!("{sha}-dirty")
                } else {
                    sha
                }
            }
            None => "unknown".to_string(),
        },
    };
    println!("cargo:rustc-env=TQ_GIT_SHA={sha}");
}
