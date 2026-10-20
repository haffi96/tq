<p align="center"><img src="assets/brand/icon.png" alt="tq" width="128" /></p>

# tq: teleop over QUIC

Low-latency robot teleoperation, video and control, over WebTransport (QUIC)
instead of WebRTC.

- Pre-encoded H.264/H.265 camera streams arrive over TCP and are never
  decoded or re-encoded on the way. (Locally the encoder is `gstreamer-source`
  from [streaming-tools](https://github.com/haffi96/streaming-tools).)
- One relay serves the robot, the operator and any number of viewers.
- The browser decodes with WebCodecs and draws every frame as soon as it is
  decoded. There is no jitter buffer.
- Teleop runs over QUIC datagrams, with a dead-man switch on the robot side.

```text
 Robot (Starlink)                            Cloud relay                         Browsers
 encoder --TCP--> tq-publisher ==QUIC/WT==> tq-relay ==WebTransport==> operator (drops aggressively)
 H.264/H.265      AU assembly,   GOP streams  GOP cache, fan-out,        \=> viewers (looser policy)
 Annex B / AVC    teleop sink    datagrams    claim/lease, auth, HTTP
                  + dead-man   <==teleop====  forwards holder's datagrams
```

## Layout

| Path | What it is |
| --- | --- |
| `crates/tq-media` | Elementary stream handling: NAL framing (Annex B or 4-byte length-prefixed, auto-detected), access unit assembly, SEI capture timestamps, codec strings, parameter set repetition. No decoding. |
| `crates/tq-proto` | Wire format: GOP stream and frame headers, JSON control messages, teleop datagrams, HS256 tokens. `web/src/protocol.ts` mirrors it. |
| `crates/tq-transport` | quinn / web-transport-quinn endpoints (BBR, bounded send window), certificates, the per-camera GOP cache (`Track`), the GOP stream sender with its drop policy, and the reader. |
| `crates/tq-publisher` | Robot-side binary: reads the TCP sources, uplinks to the relay, runs the teleop sink and dead-man. |
| `crates/tq-relay` | Relay binary: WebTransport for the publisher and browsers, HTTP for the app, `/api/cert` and dev tokens. |
| `crates/tq-tools` | `tq-impair` (Starlink-like UDP impairment proxy) and `tq-probe` (headless operator/viewer that reports per-hop latency). |
| `crates/tq-app` | Native operator/viewer app (GPUI; VideoToolbox on macOS, NVDEC on Linux), replacing `web/`. Not built by a plain `cargo build`. |
| `web/` | Browser app in TypeScript, built with bun. `worker.ts` owns WebTransport, WebCodecs and OffscreenCanvas; `main.ts` is the UI. |
| `config/` | Publisher configs: `publisher.example.json` (H.264 + H.265 test sources), `publisher.prodlike.json` (one H.265 camera, 10 fps, 2 Mbps, length-prefixed, like a typical robot camera). |
| `dev/run-local.sh` | Starts the whole stack on one machine. |
| `dev/release.sh` | Bumps a component's version; CI uses it to check tags and write release notes. |
| `build/version.rs` | Build script shared by the binaries: stamps the git commit into `--version` and the handshake. |
| `packaging/` | Publisher tarball/`.deb`/`install.sh` and service files, the relay's Dockerfile, the app's `.dmg` bundler. |
| `infra/gcp/` | Terraform for the relay on GCP (London VM, Let's Encrypt, Cloudflare DNS). The web app deploys to Cloudflare from `web/` with wrangler. |
| `assets/brand/` | The logo (`source.png`) and `make.py`, which derives the favicon, app icons, GitHub avatar and social preview from it. |
| `docs/install.md` | Installing, upgrading and releasing each component; versions and protocol compatibility. |

## Versions and releases

The publisher, the relay (with the web app inside) and tq-app are released
separately, each from its own tag (`publisher-v*`, `relay-v*`, `app-v*`).
They only need to agree on the protocol's major version. Every session
exchanges each side's version, commit and protocol. A different minor shows
a warning; a different major is refused. tq-app's status bar and the web
page's header show which app, relay and publisher versions are in use. See
[docs/install.md](docs/install.md).

## Running locally

Prerequisites:
- Rust 1.85 or newer
- bun
- Chrome or Edge, version 120 or newer. HEVC needs a hardware decoder (VideoToolbox on macOS).
- [streaming-tools](https://github.com/haffi96/streaming-tools), checked out next to this repo
  (or set `STREAMING_TOOLS=/path/to/streaming-tools`). It provides `gstreamer-source`, the test encoder.

```bash
dev/run-local.sh                    # direct
IMPAIR=starlink dev/run-local.sh    # robot uplink through the Starlink profile
IMPAIR=starlink-bad dev/run-local.sh
```

Then open:
- operator: http://localhost:8080/?av=robot-01&role=operator
- viewer: http://localhost:8080/?av=robot-01&role=viewer

Operator controls:
- **Claim**, then hold `W`/`A`/`S`/`D` to drive (commands at 20 Hz).
- **stream 20 Hz** keeps commands flowing without a key held.
- `Space` releases the claim, so the robot trips immediately.

### Native app (macOS, Linux)

`tq-app` is the native operator console. GPUI draws the window, with
components from [gpui-kit](https://github.com/longbridge/gpui-kit).
The protocol runs in-process on tokio, using `tq-transport` like `tq-probe`
does. H.264 and H.265 decode in hardware:
- macOS: VideoToolbox decodes into NV12 IOSurfaces, and GPUI's `surface()`
  element composites those without a copy.
- Linux: NVDEC decodes on an NVIDIA GPU (the driver's libraries are loaded
  at runtime; no CUDA toolkit needed). GPUI has no video surface on Linux,
  so each frame is copied to the host, converted to BGRA and drawn as an
  image. Other GPUs (VA-API) are not supported yet.

On Linux, GPUI needs these to build (Ubuntu):

```bash
sudo apt install libfontconfig-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev libvulkan-dev
```

gpui-kit pins a `gpui-pre` snapshot that needs Rust 1.95 or newer:

```bash
rustup update stable                  # or: rustup toolchain install 1.95, then cargo +1.95 …
cargo run --release -p tq-app -- --role operator      # or --role viewer
cargo run --release -p tq-app -- --role viewer --log-stats   # also log per-camera numbers
```

The window is a dock console:
- The camera grid is in the middle, with a latency chip on each tile.
- The events panel is underneath.
- A resizable sidebar holds the operator controls, glass latency with a 60 s
  sparkline per camera, and the link.
- A status bar is at the bottom.

The title bar toggles the panels and switches between light and dark. The
app follows the system appearance until you switch.

Video frames do not redraw the rest of the window. Every part is a cached
view. A decoded picture notifies only its camera's `Video` view (the
surface alone). The chips over it (`Hud`) and the panels redraw with the
2 Hz stats tick, control messages and key presses. Two rules keep it so:
- A cached view is sized by the style it is given, not by its content.
- gpui re-renders every cached view inside a cached view that re-renders.
  So the path from the root to the video, through the grid, stays uncached.

The status bar shows the UI's own cost per frame, from render start to the
last element painted. `RUST_LOG=tq_app::frame=trace` logs it for every
frame.

It reads the certificate hash and the QUIC port from `--relay-http`
(default `http://127.0.0.1:8080`). For the token it takes `--token`;
otherwise it signs one with `TQ_SHARED_SECRET` or asks a `--dev` relay.
Started from Finder (the `.dmg` build) or a desktop launcher, it takes the
same settings from `~/Library/Application Support/tq-app/config.json`
(macOS) or `$XDG_CONFIG_HOME/tq-app/config.json` (default
`~/.config/tq-app/config.json`) on Linux, and it updates itself
from GitHub Releases ([docs/install.md](docs/install.md#native-app-tq-app)).

The `.dmg` app is signed ad hoc and not notarized, so macOS blocks the first
launch ("Apple could not verify tq-app…"), and on macOS 15 or newer
right-click → **Open** doesn't get past it. After dragging it to
Applications, either open it once, click **Done**, then click **Open Anyway**
under **System Settings → Privacy & Security**, or run:

```bash
xattr -dr com.apple.quarantine /Applications/tq-app.app
```

The operator controls match the web page: Claim / Force claim / Release,
hold `W`/`A`/`S`/`D`, and `Space` for e-stop.

`tq-app` is a workspace member but not a default member, because GPUI is a
large build. Use `-p tq-app`.

Each tile's latency breakdown (the ⓘ button on hover) replaces the web
page's `relay→browser` with `relay→app`. In place of `capture→drawn` it
shows three lines:
- `capture→decoded`: VideoToolbox handed back the picture.
- `capture→render`: the picture was painted into the frame.
- `decoded→painted`: what the UI adds.

Most of `decoded→painted` is the wait for the next display-link tick. gpui
draws on CVDisplayLink. So at 60 Hz, `decoded→painted` is about 1 ms of
work plus 0 to 16.7 ms of waiting. A 30 fps source drifts slowly against
vsync, so the wait changes from one run to the next. There is no scan-out
estimate yet.

Doing it by hand:

```bash
export TQ_SHARED_SECRET=dev-secret-change-me
cargo build --release
(cd web && bun install && bun run build)
./target/release/tq-relay --dev --web-dir web/dist                      # QUIC :4443, HTTP :8080
./target/release/tq-publisher --config config/publisher.example.json
./target/release/tq-probe --role operator --teleop-hz 20 --duration 20   # headless check
```

Tokens: `tq-relay token --role operator --av robot-01` prints a token. Pass
it to the page as `?token=`. Without `--dev`, the relay does not mint tokens
over HTTP.

### Certificates

When no `--cert/--key` is given, the relay generates a self-signed ECDSA
certificate valid for 10 days. Browsers then pin it with
`serverCertificateHashes`: the page reads the hash from `/api/cert`, and the
publisher uses `relay_trust: hash_url`. The page itself must be a secure
context, which means either `http://localhost` or HTTPS. For a real
deployment, give the relay a Let's Encrypt certificate (`--cert/--key`) and
use `relay_trust: system_roots`.

### Source framing

The publisher completes an access unit as soon as it knows the AU is over:

- A **length-prefixed** NAL is complete as soon as its last byte arrives.
- An **Annex B** NAL is only complete when the next start code arrives.
- An **AUD** (fixed size) closes the current access unit immediately.
  `gstreamer-source --au-terminator`, on by default, appends one to every
  AU. Without it, each frame waits until the next frame starts, which is a
  full frame interval: 100 ms at 10 fps.
- Encoders that emit one slice per frame and no AUD terminator
  can set `"complete_on_vcl": true` on the camera to get the same effect.

## Protocol (v1)

Every hop is a WebTransport session with three kinds of channel.

**1. Control stream.** This is the first bidirectional stream, opened by the
client. Each message is `len:u32` followed by JSON with a `type` field.
- Publisher: `announce` and `publisher_status`.
- Browser: `claim`, `release`, `keyframe_request`.
- Relay: `welcome`, `state`, `claim_result`, `link_stats`, `operator`.
- Both sides use `ping`/`pong` to measure RTT and NTP-style clock offset.

**2. GOP streams.** There is one unidirectional stream per camera per GOP:

```text
type=0x01 ver=1 camera:u16 group:u64 codec_len:u8 codec
then per frame: len:u32 index:u32 flags:u8 capture_us:u64 publisher_us:u64 relay_us:u64 frame_id:u32 payload(Annex B AU)
```

A group starts with its keyframe, which carries its parameter sets. Newer
groups get a higher QUIC stream priority. Each hop renumbers groups so they
stay monotonic across publisher restarts.

**3. Datagrams.** Teleop `command` (0x10), `ack` (0x11), `heartbeat` (0x12)
and `echo` (0x13). Each carries a sequence number and the sender's own
timestamp, so RTT needs no clock sync. The relay forwards them only to and
from the session that holds the operator claim.

### Why one stream per GOP

In an IPPP stream, every frame depends on the one before it. Delivering
frames out of order therefore buys nothing, and per-frame streams would only
add reordering work in the browser.

A stream per GOP gives in-order delivery inside the GOP, and lets a sender
drop a whole stale GOP with a single `RESET_STREAM`. This is the group model
from MoQ.

### Drop policy (`GroupSender`)

| Hop | Old GOP when a new keyframe arrives | Late frame |
| --- | --- | --- |
| publisher → relay | reset immediately | dropped once older than `max_lag_ms` (400); skip to next keyframe |
| relay → operator | reset immediately | `--operator-max-lag-ms` (250) |
| relay → viewer | `--viewer-keep-groups` (1) older GOPs may finish | `--viewer-max-lag-ms` (1500) |

"Late" is measured from when the frame arrived on this node. Writes are
bounded by the QUIC `send_window` (1 MiB). Once it is full, a frame can't
sit inside QUIC for seconds, so it blocks, ages past the limit and gets
dropped.

On the browser side, a group stream that is superseded is cancelled. A
decoder that has held a frame for more than 150 ms skips to the next
keyframe.

There is no encoder control hook: the encoder is a separate process. A skip
therefore recovers at the encoder's periodic IDR (1 s in gstreamer-source).
Keyframe requests are still sent all the way to the publisher, which logs
them, so an encoder hook can be added there.

### Teleop and dead-man (publisher)

Commands are latest-wins by `seq`. The robot is safe-stopped (tripped)
whenever any of these is true:
- there is no operator, or the operator changes;
- no heartbeat has arrived for `heartbeat_loss_ms` (500);
- commands have started and then stopped for `command_stale_ms` (500);
- the relay connection drops.

It stays tripped until the next accepted command. `LoggingSink` stands in
for the robot interface (`TeleopSink`).

## Measuring

Both the page overlay and `tq-probe` report latency per hop. Each line is
`avg/max` over the last 0.5 s.

| Line | Measured from → to | Clock(s) |
| --- | --- | --- |
| `capture→pub` | SEI capture timestamp → publisher has the whole access unit. Covers the camera pipeline after the driver timestamp, encode, TCP and AU assembly. | robot only |
| `pub→relay` | publisher → relay received the frame (the uplink) | robot ↔ relay offset |
| `relay→browser` | relay → the frame's last byte read in the worker | relay ↔ browser offset |
| `decode` | `VideoDecoder.decode()` → output callback | browser only, exact |
| `capture→drawn` | capture → `drawImage` returned | both offsets |
| `draw→display≈` | `drawImage` → estimated scan-out: the next animation frame plus one refresh interval. Panel response time is not included. | browser only, estimate |
| `glass→glass≈` | capture → estimated scan-out, plus `?sensor_ms=` | both offsets |

The `±… sync` figure is the worst-case clock error. The relay pings the
publisher, and the browser pings the relay, every 500 ms. Each side keeps
the last 20 samples and uses the one with the smallest round trip (the NTP
clock-filter idea), because queueing in one direction biases the offset by
up to half the round trip. Half that best round trip bounds the error, and
the two hops add up. On the emulated Starlink uplink the bound is about
±23 ms, while the actual error measured on one machine is about 2.5 ms.

**What the camera timestamp misses:** exposure, sensor readout and USB/ISP
transfer before the driver stamps the frame. Measure it once optically (see
below) and pass the result as `?sensor_ms=`.

### Glass to glass without clock sync

- **Optical, one clock.** Point the robot camera at a millisecond clock
  that sits next to the operator display, then take one photo (or a phone
  slow-motion video) of both. The difference between the two clock readings
  is the true glass to glass. Use it to calibrate `?sensor_ms=`.
- **Command to photon.** The operator sends a teleop command that changes
  something the camera sees (an LED, or a burned-in marker), and the page
  detects that change in the decoded video. This is a round trip on the
  browser's clock alone, so no sync is needed, and it is the latency an
  operator actually feels.
- **Disciplined clocks.** GPS PPS + chrony on the robot, and chrony
  against the cloud provider's time service on the relay and operator
  machines. This gets the wall clocks within about 1 ms, and the in-band
  offsets then only cross-check them.

Results from 2026-10-06 on one MacBook: VideoToolbox test sources, links
emulated with `tq-impair` on the publisher uplink only, 45 s per run. These
are emulated links, not real Starlink.

| Source | Link | capture→probe p50 / p95 | pub→relay p50 / p95 | teleop RTT p50 / p95 |
| --- | --- | --- | --- | --- |
| H.264 + H.265 720p30, ~100 kbps | none | 12 / 18 ms | 0.3 / 0.6 ms | 1.1 / 2.5 ms |
| H.264 + H.265 720p30, ~100 kbps | starlink | 40 / 48 ms | 28 / 35 ms | 56 / 67 ms |
| H.265 720p10, 2 Mbps (snow) | none | 19 / 26 ms | 2.5 / 4.5 ms | 4 / 6 ms |
| H.265 720p10, 2 Mbps (snow) | starlink | 61 / 123 ms | 45 / 104 ms | 61 / 114 ms |
| H.265 720p10, 2 Mbps (snow) | starlink-bad | 89 / 268 ms | 73 / 243 ms | 90 / 222 ms |

Chrome 154, operator page, 2 Mbps H.265 over the `starlink` profile: decode
2 ms and capture→drawn about 70 ms on average (111 ms max).

The profiles:
- `starlink`: 25 ms ±8 ms each way, 0.3% random loss plus Gilbert-Elliott
  bursts, a 10 Mbps uplink, and every 15 s a 200 ms handover with +40 ms and
  5% loss.
- `starlink-bad`: 35 ms ±15 ms, 1% loss, a 4 Mbps uplink, and handovers of
  400 ms with +80 ms and 20% loss.

At 2 Mbps and 10 fps an average frame is about 25 KB, which takes about
20 ms to cross a 10 Mbps uplink. Frame size matters on Starlink as much as
RTT does.

## Not done yet

- No encoder hook. Keyframe requests are logged, and recovery waits for the
  periodic IDR.
- No WebSocket/qmux fallback for browsers without WebTransport (Safari).
- Only the relay can mint tokens. There is no registry or discovery: one
  relay serves robots by id from the URL.
- Relay HTTP is plain HTTP. Remote browsers need HTTPS in front of it, or
  a real certificate.
