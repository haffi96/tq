# Installing and releasing tq

Each part of tq is versioned and released on its own:

| Component | Version in | Tag | What a release contains |
| --- | --- | --- | --- |
| Publisher | `crates/tq-publisher/Cargo.toml` | `publisher-vX.Y.Z` | Tarballs for Linux x86_64, Linux aarch64 and macOS arm64, `.deb` files for Linux, `install.sh`, `SHA256SUMS` |
| Relay | `crates/tq-relay/Cargo.toml` | `relay-vX.Y.Z` | The container image `ghcr.io/haffi96/tq-relay:X.Y.Z`, with the web app inside |
| Web app | `web/package.json` | none | Nothing on its own. It ships inside the relay image. |
| Native app | `crates/tq-app/Cargo.toml` | `app-vX.Y.Z` | macOS: `.dmg` to install, `.zip` for the in-app updater. Linux: `.tar.gz` for both. `SHA256SUMS` over all of them. |

The libraries (`tq-proto`, `tq-transport`, `tq-media`) are not released.
Each binary includes whatever version of them it was built with.

## What has to match: the protocol

Each component has its own version. Components only need to agree on the
**protocol version** (`PROTOCOL` in `crates/tq-proto/src/version.rs`,
mirrored in `web/src/protocol.ts`):

- **Minor** (1.1 → 1.2): adds fields or messages that older peers ignore.
  A minor mismatch is shown as a warning in amber, and everything else keeps
  working.
- **Major** (1.x → 2.0): a change older peers cannot ignore. The relay
  refuses a peer with a different major. It closes the session with code 6
  and a reason that names both versions. tq-app and the web page check
  `/api/info` first and do not connect to an incompatible relay.

Every session starts with each side's build: version, commit and protocol.
- The publisher sends its build in `announce`.
- Clients send theirs in `hello`.
- The relay sends its build in `welcome` and `announce_ok`, and serves it at
  `/api/info`.

Peers from before protocol 1.1 send no build. They count as protocol 1.0.

Protocol 2.0 introduced rooms and the events stream (and renamed `av` to
`room` in URLs, tokens and the publisher's `room_id`), so 1.x peers and
2.0 peers refuse each other: upgrade the relay, publishers and apps
together.

**Rollout order:** relay first, then publishers and apps. The relay accepts
any minor of its own major, so publishers and apps can follow at their own
pace.

### Seeing versions

- **tq-app:**
  - The status bar reads `app 0.1.0 · relay 0.1.0 · publisher 0.1.0`. Each
    entry turns amber on a protocol mismatch.
  - The sidebar's **About** section shows each component's commit and
    protocol.
  - The events panel logs each version when a session connects.
- **Web page:** the `versions` pill in the header. Hover over it to see the
  commits.
- **CLIs:** `tq-relay --version`, `tq-publisher --version` and
  `tq-app --version` print `0.1.0 (a1b2c3d4e5) protocol 2.0`.
  A `-dirty` suffix on the commit means the binary was built from a tree
  with uncommitted changes.
- **Relay HTTP:**
  - `GET /api/info` returns `build`, the relay's own build.
  - `GET /api/rooms` includes each publisher's `build`.
- **Logs:** each binary logs its build at startup. The relay logs the build
  of every publisher and client that connects.

## Releasing

```sh
dev/release.sh bump relay 0.2.0     # edits the version (and Cargo.lock); does not commit
git commit -am "relay 0.2.0"
git tag relay-v0.2.0
git push origin HEAD relay-v0.2.0
```

The tag starts that component's workflow
(`.github/workflows/release-{publisher,relay,app}.yml`). The workflow:
1. Checks that the tag matches the version in the source.
2. Builds, tests and packages the component.
3. Creates a GitHub release. Its notes list the commits that touched the
   component since its previous tag.

Releases are created with `--latest=false`, because "latest" means nothing
across several components.

After the first relay release, check that `ghcr.io/haffi96/tq-relay` is public:
a new GHCR package can start out private. If `docker pull` asks you to log in,
make it public under **Packages → tq-relay → Package settings → Change
visibility**.

The web app has no tag. To ship a web change, bump `web/package.json` if you
want the version to change (`dev/release.sh bump web X.Y.Z`), then release
the relay.

## Tokens

Releases are public, so neither the installer nor the app needs a GitHub
token. If one is available, they use it:
- `GITHUB_TOKEN` (installer) or `TQ_UPDATE_TOKEN` (app)
- `update.token` in the app's config file
- `gh auth token`, if you are logged in with `gh`

A token raises GitHub's API rate limit, which matters when many machines
install at once from one address. It is also what lets a private fork work.

## Publisher

The publisher has no GPU dependency: it forwards streams that are already
encoded. It is one binary per platform, built for glibc 2.31 and newer, so
it runs on:
- Linux aarch64: desktops with a discrete GPU, Jetson Orin Nano
  (JetPack 5 and 6), Jetson Thor (JetPack 7)
- Linux x86_64
- macOS on Apple silicon

### Option 1: install script (Linux and macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/haffi96/tq/main/packaging/publisher/install.sh | bash
```

Options go after `bash -s --`, for example
`… | bash -s -- --version 0.2.0`:

| Option | Effect |
| --- | --- |
| `--version X.Y.Z` | A specific release. Default: the newest `publisher-v*`. |
| `--file tq-publisher-….tar.gz` | Install from a tarball you downloaded. No network. |
| `--prefix DIR` | The binary goes to `DIR/bin`. Default `/usr/local`. |
| `--config-dir DIR` | Default `/etc/tq`. |
| `--no-service` | Do not install or restart the service. |
| `--uninstall` | Stop the service and remove the binary. Keeps the config. |

The script:
1. Picks the tarball for this OS and CPU.
2. Checks it against `SHA256SUMS`.
3. Installs `tq-publisher` to `/usr/local/bin`.
4. On first install only, creates `/etc/tq/publisher.json` from the example
   and `/etc/tq/publisher.env` for secrets. Later installs leave both alone.
5. Installs the service:
   - **Linux:** a systemd unit, `tq-publisher.service`, running as the `tq`
     system user.
   - **macOS:** a launchd daemon, `/Library/LaunchDaemons/dev.tq.publisher.plist`,
     logging to `/var/log/tq-publisher.log`.
6. On a first install, it tells you to edit the config and start the
   service. On an upgrade, it restarts the service.

Run the same command again to upgrade.

### Option 2: .deb (Ubuntu, Debian, JetPack)

```sh
curl -fLO https://github.com/haffi96/tq/releases/download/publisher-v0.2.0/tq-publisher_0.2.0_arm64.deb   # or _amd64
sudo apt install ./tq-publisher_0.2.0_arm64.deb
```

The `.deb` installs the same binary to `/usr/bin` and the unit to
`/lib/systemd/system`. It creates the `tq` user and seeds `/etc/tq` once,
the same way the script does. Upgrades restart the service if it is
running. `apt purge tq-publisher` removes `/etc/tq`.

Use one method per machine. The script refuses to run while the `.deb` is
installed, unless you pass `--force`.

### Configure and run

1. Edit `/etc/tq/publisher.json`. See `config/publisher.example.json` and
   `config/publisher.prodlike.json`: set `room_id`, `relay_url`,
   `relay_trust` and the cameras.
2. Put the token secret in `/etc/tq/publisher.env`:
   ```sh
   TQ_SHARED_SECRET=…
   ```
3. Start it:
   - Linux: `sudo systemctl start tq-publisher`, and follow the logs with
     `journalctl -u tq-publisher -f`.
   - macOS: `sudo launchctl bootstrap system /Library/LaunchDaemons/dev.tq.publisher.plist`.

### Building the packages by hand

```sh
cargo zigbuild --release -p tq-publisher --target aarch64-unknown-linux-gnu.2.31   # brew install zig; cargo install cargo-zigbuild
packaging/publisher/package.sh 0.2.0 linux-aarch64 target/aarch64-unknown-linux-gnu/release/tq-publisher dist
```

zig is only needed to build. It cross-compiles the C code in the TLS
library and links against glibc 2.31, so a single build covers every
supported Ubuntu and JetPack release.

## Relay

The relay ships as a container image. The web app is served from
`/usr/share/tq/web`.

```sh
docker run -d --name tq-relay --restart unless-stopped \
  -p 8080:8080 -p 4443:4443/udp \
  -e TQ_SHARED_SECRET=… \
  ghcr.io/haffi96/tq-relay:0.2.0 --hostnames relay.example.com
```

| Setting | Default in the image |
| --- | --- |
| `TQ_QUIC` | `0.0.0.0:4443`, WebTransport over UDP |
| `TQ_HTTP` | `0.0.0.0:8080`, the web app, `/api/*` |
| `TQ_WEB_DIR` | `/usr/share/tq/web` |
| `TQ_SHARED_SECRET` | required |
| `TQ_HTTP_TLS` | off. `true` serves HTTP over TLS with `--cert`/`--key` |
| `TQ_CORS_ORIGINS` | empty. Comma-separated origins allowed to call `/api/*`, e.g. `https://tq.haffi.dev` |

Publish the QUIC port on the same number it uses inside the container
(`-p 4443:4443/udp`). Apps and the web page read the port from
`/api/info`, which reports the port inside the container. If you need a
different port, change both sides: `-e TQ_QUIC=0.0.0.0:5443 -p 5443:5443/udp`.

**Certificates:**
- Without `--cert`/`--key`, the relay generates a self-signed certificate
  valid for 10 days. Browsers and apps pin it by hash.
- For a real certificate, mount it and pass both flags:
  `-v /etc/letsencrypt/live/relay:/certs:ro … --cert /certs/fullchain.pem --key /certs/privkey.pem`.

**Web app on another origin:** a page served over https (for example from
Cloudflare, `web/wrangler.jsonc`) can only call a relay that also speaks
https, and WebTransport needs the page to be a secure context. Run the relay
with a real certificate, `TQ_HTTP_TLS=true` and
`TQ_CORS_ORIGINS=https://<page origin>`. Build the page with
`TQ_RELAY_HTTP=https://<relay>` (`bun run deploy` in `web/` does), or pass
`?relay_http=` in its URL.

**GCP:** `infra/gcp/` is Terraform for one relay VM with a Let's Encrypt
certificate and a Cloudflare DNS record. See its README.

**Upgrading:** pull the new tag, then recreate the container. The relay
exits on SIGTERM (`docker stop`) and closes its sessions.

**Building locally:**

```sh
docker build -f packaging/relay/Dockerfile --build-arg TQ_GIT_SHA=$(git rev-parse --short=10 HEAD) -t tq-relay .
```

## Native app (tq-app)

### Installing on macOS

1. Download `tq-app-X.Y.Z-macos-arm64.dmg` from the `app-vX.Y.Z` release:
   `https://github.com/haffi96/tq/releases` (or `gh release download app-v0.2.0 -R haffi96/tq -p '*.dmg'`).
2. Drag **tq-app** to Applications.
3. The app is signed ad hoc and not notarized yet, so macOS blocks it the
   first time ("Apple could not verify tq-app…"). Right-click → **Open** no
   longer gets past this on macOS 15 (Sequoia) or newer. Either:
   - open it once, click **Done**, then go to **System Settings → Privacy &
     Security**, scroll down, click **Open Anyway** and confirm, or
   - run `xattr -dr com.apple.quarantine /Applications/tq-app.app`.

   Updates installed from inside the app don't need this again.

### Installing on Linux

Needs x86_64, glibc 2.39 or newer (Ubuntu 24.04 and up), and an NVIDIA GPU
with its driver: video decodes with NVDEC. Other GPUs are not supported yet.

1. Download `tq-app-X.Y.Z-linux-x86_64.tar.gz` from the `app-vX.Y.Z` release
   (or `gh release download app-v0.2.0 -R haffi96/tq -p '*linux*'`).
2. Unpack it and put `tq-app` somewhere you can write to, so it can update
   itself. `~/.local/bin` works:

   ```sh
   tar -xzf tq-app-*-linux-x86_64.tar.gz
   install -D tq-app-*-linux-x86_64/tq-app ~/.local/bin/tq-app
   install -Dm644 tq-app-*-linux-x86_64/tq-app.desktop ~/.local/share/applications/tq-app.desktop   # optional launcher
   install -Dm644 tq-app-*-linux-x86_64/tq-app.png ~/.local/share/icons/hicolor/512x512/apps/tq-app.png   # its icon
   ```

### Settings

Started from Finder or a desktop launcher, the app gets no command-line
flags. It reads its settings from a file instead; flags still override it:
- macOS: `~/Library/Application Support/tq-app/config.json`
- Linux: `$XDG_CONFIG_HOME/tq-app/config.json` (default `~/.config/tq-app/config.json`)

```json
{
  "relay_http": "https://tq-relay-1.haffi.dev",
  "room": "robot-01",
  "role": "operator",
  "secret": "…"
}
```

| Key | Meaning |
| --- | --- |
| `relay_http` | Relay HTTP base. Default `http://127.0.0.1:8080`. Use `https://` for a relay with a real certificate (checked against the system CA roots), and `http://` for a self-signed or dev relay. |
| `relay` | QUIC `host:port`. Default: the HTTP host and the relay's QUIC port. |
| `av`, `role` | Default `robot-01`, `viewer`. |
| `token` or `secret` | A session token, or the shared secret to sign one. Without either, the app asks a `--dev` relay for a token. |
| `update.repo`, `update.prerelease` | Where updates come from (default `haffi96/tq`), and whether to offer pre-releases. |
| `update.token` | Optional GitHub token (see [Tokens](#tokens)). |

### Updating

The app checks for a newer `app-v*` release when it starts. When one exists:
- **Update to X.Y.Z** appears in the title bar.
- **Install X.Y.Z and restart** appears under **About** in the sidebar.
- **Check for updates** in the same section checks again at any time.

Installing an update:
1. Downloads the release archive for this platform (`.zip` on macOS,
   `.tar.gz` on Linux).
2. Checks it against `SHA256SUMS`. On macOS it also checks the bundle's
   version and verifies its code signature. On Linux it runs the new
   binary's `--version`, which checks the version and that it runs here.
3. Swaps it for the running app, in place.
4. Restarts with the same arguments.

Nothing is replaced if any check fails. Two limits:
- Updating is disabled while you hold the operator claim. The restart drops
  the session, which would trip the robot's dead-man switch mid-drive.
- The app has to be somewhere you can write to, such as `/Applications` for
  an admin user on macOS, or `~/.local/bin` on Linux. A build run from
  cargo's `target/` does not update itself.

**From a terminal:**
- `tq-app --check-update` reports whether a newer release exists.
- `tq-app --install-update` installs it without restarting.

`TQ_UPDATE_TOKEN`, `TQ_UPDATE_REPO`, `TQ_UPDATE_API` and
`TQ_UPDATE_PRERELEASE` override the config file. `TQ_UPDATE_API` exists for
testing against a mock of the GitHub API.

### Building the release files by hand

```sh
packaging/app/bundle.sh   # macOS → dist/app/tq-app.app, …-macos-arm64.zip, …-macos-arm64.dmg
packaging/app/linux.sh    # Linux → dist/app/tq-app-…-linux-x86_64.tar.gz
```

Set `CODESIGN_IDENTITY` to sign with a Developer ID instead of ad hoc.
Notarization is not set up yet.
