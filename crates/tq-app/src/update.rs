//! Self-update from GitHub Releases.
//!
//! Releases of the app are tagged `app-vX.Y.Z` and carry one archive per
//! platform plus `SHA256SUMS`:
//! - macOS: `tq-app-X.Y.Z-macos-arm64.zip`, the bundle made with `ditto`.
//!   An update checks the hash, the bundle's version and its code
//!   signature, then swaps the bundle.
//! - Linux: `tq-app-X.Y.Z-linux-x86_64.tar.gz`, the binary. An update
//!   checks the hash and runs the new binary's `--version`, then renames it
//!   over the running one.
//!
//! The new build starts once this process has exited.
//!
//! Requests go through `/usr/bin/curl`, so the app needs no TLS client of
//! its own. No token is needed; one from `TQ_UPDATE_TOKEN` / `GITHUB_TOKEN`,
//! the config file or `gh auth token` is used when present (higher API rate
//! limit, or a private fork).

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context};
use semver::Version;
use sha2::{Digest, Sha256};

pub const TAG_PREFIX: &str = "app-v";
pub const DEFAULT_REPO: &str = "haffi96/tq";
const DEFAULT_API: &str = "https://api.github.com";
const CURL: &str = "/usr/bin/curl";

/// Where updates come from; resolved once at startup.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `owner/name` on GitHub.
    pub repo: String,
    /// API base, `https://api.github.com` (a mock server in tests).
    pub api: String,
    pub token: Option<String>,
    /// Also offer releases marked pre-release on GitHub or with a semver
    /// pre-release part.
    pub prerelease: bool,
}

impl Settings {
    /// Environment first, then the config file, then defaults. The token
    /// falls back to the GitHub CLI's.
    pub fn resolve(file: &crate::config::Update) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let token = env("TQ_UPDATE_TOKEN")
            .or_else(|| env("GITHUB_TOKEN"))
            .or_else(|| file.token.clone().filter(|t| !t.is_empty()))
            .or_else(gh_token);
        Self {
            repo: env("TQ_UPDATE_REPO")
                .or_else(|| file.repo.clone())
                .unwrap_or_else(|| DEFAULT_REPO.into()),
            api: env("TQ_UPDATE_API")
                .or_else(|| file.api.clone())
                .unwrap_or_else(|| DEFAULT_API.into())
                .trim_end_matches('/')
                .to_string(),
            token,
            prerelease: env("TQ_UPDATE_PRERELEASE").is_some() || file.prerelease,
        }
    }
}

/// `gh auth token`, looking where Homebrew installs it: an app started from
/// Finder does not get the shell's PATH.
fn gh_token() -> Option<String> {
    for gh in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "gh"] {
        let Ok(out) = Command::new(gh).args(["auth", "token"]).stderr(Stdio::null()).output() else {
            continue;
        };
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !token.is_empty() {
            return Some(token);
        }
    }
    None
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    /// API URL; downloads with `Accept: application/octet-stream`, which
    /// works for private repositories too.
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub page: String,
    /// This platform's archive.
    pub archive: Asset,
    pub sums: Option<Asset>,
}

/// The release archive for this platform.
#[cfg(target_os = "macos")]
pub fn archive_name(v: &Version) -> String {
    format!("tq-app-{v}-macos-arm64.zip")
}

/// The release archive for this platform.
#[cfg(target_os = "linux")]
pub fn archive_name(v: &Version) -> String {
    format!("tq-app-{v}-linux-{}.tar.gz", std::env::consts::ARCH)
}

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    url: String,
}

/// The newest app release, if it is newer than this build.
pub fn check(s: &Settings) -> anyhow::Result<Option<Release>> {
    let body = curl(
        s,
        &format!("{}/repos/{}/releases?per_page=100", s.api, s.repo),
        "application/vnd.github+json",
        None,
    )?;
    let releases: Vec<GhRelease> = serde_json::from_slice(&body).context("GitHub releases response")?;
    Ok(newest(releases, s.prerelease).filter(|r| r.version > current()))
}

fn newest(releases: Vec<GhRelease>, prerelease: bool) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft && (prerelease || !r.prerelease))
        .filter_map(|r| {
            let version = Version::parse(r.tag_name.strip_prefix(TAG_PREFIX)?).ok()?;
            if !prerelease && !version.pre.is_empty() {
                return None;
            }
            let asset = |name: &str| {
                r.assets.iter().find(|a| a.name == name).map(|a| Asset {
                    name: a.name.clone(),
                    url: a.url.clone(),
                })
            };
            Some(Release {
                archive: asset(&archive_name(&version))?,
                sums: asset("SHA256SUMS"),
                tag: r.tag_name.clone(),
                page: r.html_url.clone(),
                version,
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The `.app` this process runs from.
#[cfg(target_os = "macos")]
pub fn install_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    // X.app/Contents/MacOS/tq-app
    let bundle = exe
        .ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .context("not running from an .app bundle (built with cargo run?); install the .dmg to update")?;
    Ok(bundle.to_path_buf())
}

/// The binary this process runs from.
#[cfg(target_os = "linux")]
pub fn install_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let parent = exe.parent().context("binary has no parent")?;
    if parent.ends_with("target/debug") || parent.ends_with("target/release") {
        bail!("built with cargo; install a release tarball to update");
    }
    Ok(exe)
}

/// Leftovers of an update that died half way, next to the install.
pub fn clean_stale_staging() {
    let Ok(installed) = install_path() else { return };
    let Some(parent) = installed.parent() else { return };
    let Ok(dir) = std::fs::read_dir(parent) else { return };
    for e in dir.flatten() {
        if e.file_name().to_string_lossy().starts_with(".tq-app-update-") {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Downloads, verifies and swaps in `release`. Returns the install path; the
/// running process is still the old build until it restarts.
pub fn install(s: &Settings, release: &Release, progress: &dyn Fn(&str)) -> anyhow::Result<PathBuf> {
    let installed = install_path()?;
    let parent = installed.parent().context("install path has no parent")?;
    // Same file system as the install, so the swap is renames.
    let stage = parent.join(format!(".tq-app-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir(&stage).with_context(|| {
        format!(
            "cannot write next to {} ({})",
            installed.display(),
            if cfg!(target_os = "macos") {
                "move the app to a folder you can write to"
            } else {
                "install tq-app somewhere you can write to, such as ~/.local/bin"
            }
        )
    })?;
    let result = (|| {
        progress(&format!("Downloading {}", release.archive.name));
        let archive = stage.join(&release.archive.name);
        curl(s, &release.archive.url, "application/octet-stream", Some(&archive))?;

        match &release.sums {
            Some(sums) => {
                let text = curl(s, &sums.url, "application/octet-stream", None)?;
                let expected = parse_sums(&String::from_utf8_lossy(&text), &release.archive.name)
                    .with_context(|| format!("{} not listed in SHA256SUMS", release.archive.name))?;
                let actual = sha256_file(&archive)?;
                if !actual.eq_ignore_ascii_case(&expected) {
                    bail!(
                        "checksum mismatch for {}: expected {expected}, got {actual}",
                        release.archive.name
                    );
                }
            }
            None => bail!("release {} has no SHA256SUMS", release.tag),
        }

        progress("Unpacking");
        let unpacked = stage.join("new");
        let new = unpack(&archive, &unpacked, release)?;
        progress("Installing");
        swap(&new, &installed, &stage)
    })();
    // What is left can go now: the running binary stays mapped.
    let _ = std::fs::remove_dir_all(&stage);
    result.map(|()| installed)
}

/// Unpacks and checks the bundle; returns the new `.app`.
#[cfg(target_os = "macos")]
fn unpack(zip: &Path, into: &Path, release: &Release) -> anyhow::Result<PathBuf> {
    run(Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(zip).arg(into))?;
    let new_app = std::fs::read_dir(into)?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .context("no .app in the update")?;

    let plist = new_app.join("Contents/Info.plist");
    let version = output(
        Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString"])
            .arg(&plist),
    )?;
    if version.trim() != release.version.to_string() {
        bail!(
            "the update's bundle says {}, expected {}",
            version.trim(),
            release.version
        );
    }
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(&new_app))
    .context("code signature")?;
    Ok(new_app)
}

/// Unpacks the tarball and runs the new binary once; returns it.
#[cfg(target_os = "linux")]
fn unpack(tarball: &Path, into: &Path, release: &Release) -> anyhow::Result<PathBuf> {
    std::fs::create_dir(into)?;
    run(Command::new("tar").arg("-xzf").arg(tarball).arg("-C").arg(into))?;
    let new_bin = std::fs::read_dir(into)?
        .flatten()
        .map(|e| e.path().join("tq-app"))
        .find(|p| p.is_file())
        .context("no tq-app in the update")?;
    // Also proves it runs here (architecture, glibc, libraries).
    let out = output(Command::new(&new_bin).arg("--version")).context("running the new tq-app")?;
    let version = version_of(&out);
    if version != Some(release.version.to_string().as_str()) {
        bail!(
            "the update's binary says {:?}, expected {}",
            out.trim(),
            release.version
        );
    }
    Ok(new_bin)
}

/// The version in `tq-app --version` output: `tq-app 0.2.0 (commit) protocol 1.1`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn version_of(version_output: &str) -> Option<&str> {
    let mut words = version_output.split_whitespace();
    (words.next()? == "tq-app").then_some(())?;
    words.next()
}

/// Two renames, the old bundle into `stage`; puts it back if the second fails.
#[cfg(target_os = "macos")]
fn swap(new_app: &Path, bundle: &Path, stage: &Path) -> anyhow::Result<()> {
    let old = stage.join("old.app");
    std::fs::rename(bundle, &old).context("moving the running app aside")?;
    if let Err(e) = std::fs::rename(new_app, bundle) {
        let _ = std::fs::rename(&old, bundle);
        return Err(anyhow::Error::from(e).context("moving the new app in"));
    }
    Ok(())
}

/// One rename over the running binary: atomic, and the running process
/// keeps the old file until it exits.
#[cfg(target_os = "linux")]
fn swap(new_bin: &Path, bin: &Path, _stage: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(new_bin, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(new_bin, bin).context("moving the new binary in")
}

/// Starts `installed` again once this process has exited, with the same
/// arguments.
pub fn relaunch(installed: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let start = r#"exec /usr/bin/open -n "$app" --args "$@""#;
    #[cfg(target_os = "linux")]
    let start = r#"exec "$app" "$@""#;
    let script = format!(
        r#"pid=$1; app=$2; shift 2
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
{start}"#
    );
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("tq-app-relaunch")
        .arg(std::process::id().to_string())
        .arg(installed)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the relauncher")?;
    Ok(())
}

/// GET through curl. The token goes in on stdin (`-K -`), not on the command
/// line where `ps` would show it.
fn curl(s: &Settings, url: &str, accept: &str, out: Option<&Path>) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new(CURL);
    cmd.args(["-fsSL", "--retry", "2", "--connect-timeout", "10", "-K", "-"]);
    if let Some(out) = out {
        cmd.arg("-o").arg(out);
    }
    cmd.arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running curl")?;
    {
        let mut stdin = child.stdin.take().expect("piped");
        let mut config = format!(
            "header = \"Accept: {accept}\"\nheader = \"X-GitHub-Api-Version: 2022-11-28\"\nuser-agent = \"tq-app/{}\"\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(t) = &s.token {
            config.push_str(&format!("header = \"Authorization: Bearer {t}\"\n"));
        }
        stdin.write_all(config.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let hint = if err.contains("403") || err.contains("429") {
            " (GitHub rate limit: set TQ_UPDATE_TOKEN or `update.token`, or log in with `gh auth login`)"
        } else if err.contains("404") || err.contains("401") {
            if s.token.is_none() {
                " (no such repository; a private one needs a token: TQ_UPDATE_TOKEN or `update.token`)"
            } else {
                " (does the token have read access to the repository?)"
            }
        } else {
            ""
        };
        bail!("{url}: {err}{hint}");
    }
    Ok(out.stdout)
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    output(cmd).map(|_| ())
}

fn output(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The hash for `name` in `sha256sum` output (`<hex>  <name>`).
fn parse_sums(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| hash.to_string())
    })
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, pre: bool, assets: &[&str]) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: pre,
            html_url: String::new(),
            assets: assets
                .iter()
                .map(|n| GhAsset {
                    name: n.to_string(),
                    url: format!("https://api/{n}"),
                })
                .collect(),
        }
    }

    fn archive(v: &str) -> String {
        archive_name(&Version::parse(v).unwrap())
    }

    #[test]
    fn picks_newest_app_release_with_this_platforms_archive() {
        let r = newest(
            vec![
                rel("relay-v9.0.0", false, &["x"]),
                rel("app-v0.2.0", false, &[&archive("0.2.0"), "SHA256SUMS"]),
                rel("app-v0.10.0", false, &[&archive("0.10.0")]),
                rel("app-v0.11.0", false, &["tq-app-0.11.0-macos-arm64.dmg"]), // no archive
                rel("app-v0.12.0-rc.1", true, &[&archive("0.12.0-rc.1")]),
            ],
            false,
        )
        .unwrap();
        assert_eq!(r.version, Version::new(0, 10, 0));
        assert!(r.sums.is_none());

        let pre = newest(vec![rel("app-v0.12.0-rc.1", true, &[&archive("0.12.0-rc.1")])], true);
        assert_eq!(pre.unwrap().version, Version::parse("0.12.0-rc.1").unwrap());
    }

    #[test]
    fn skips_other_platforms_archives() {
        let other = if cfg!(target_os = "macos") {
            "tq-app-0.3.0-linux-x86_64.tar.gz"
        } else {
            "tq-app-0.3.0-macos-arm64.zip"
        };
        assert!(newest(vec![rel("app-v0.3.0", false, &[other, "SHA256SUMS"])], false).is_none());
    }

    #[test]
    fn version_from_version_output() {
        assert_eq!(version_of("tq-app 0.2.0 (a1b2c3d4e5) protocol 1.1\n"), Some("0.2.0"));
        assert_eq!(version_of("tq-relay 0.2.0"), None);
        assert_eq!(version_of(""), None);
    }

    #[test]
    fn sums() {
        let text = "aa11  tq-app-0.2.0-macos-arm64.dmg\nbb22  tq-app-0.2.0-macos-arm64.zip\n";
        assert_eq!(
            parse_sums(text, "tq-app-0.2.0-macos-arm64.zip").as_deref(),
            Some("bb22")
        );
        assert_eq!(parse_sums("cc33 *f.zip\n", "f.zip").as_deref(), Some("cc33"));
        assert_eq!(parse_sums(text, "nope"), None);
    }
}
