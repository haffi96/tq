<p align="center"><img src="assets/brand/icon.png" alt="tq" width="128" /></p>

# tq: tele over QUIC

Low-latency video, telemetry and control between machines and people, over
QUIC / WebTransport instead of WebRTC, with latency measured on every frame
from capture to glass. Built for robots; teleoperation is the first
complete example.

- **Broadcasts, not calls.** Any session publishes a *broadcast* (video and
  data tracks, plus *channels* others send into and *locks* that make some
  of them exclusive) and subscribes to anyone else's. What it may do comes
  from its token's permissions, down to a single track.
- **Video untouched.** Pre-encoded H.264/H.265 arrives over TCP and is never
  decoded or re-encoded on the way. One QUIC stream per GOP; stale GOPs are
  dropped whole.
- **Data as fast as it comes.** Data tracks are `latest` (newest value wins),
  `stream` (every message, in order) or `datagram` (one QUIC datagram per
  message, for IMU-rate data), and subscribers can ask the relay to thin
  them to the rate they can show.
- **Latency you can see.** Every frame and message carries capture, owner
  and relay timestamps; per-hop clock offsets put them all on the viewer's
  clock, with error bounds.
- **Teleop from general parts.** A `drive` channel behind a `drive` lock;
  the lock's watchdog is the dead-man switch.

```text
 Robot                                   Cloud relay                        Consoles
 encoder --TCP--> tq-publisher ==WT==>  tq-relay  ==WebTransport==>  web page / tq-app
 H.264/H.265      broadcast "rover":    caches, fans out per            subscribe to tracks,
                  video, .status,       subscription, checks            claim the drive lock,
                  drive lock + chan.  <== permissions and locks <==      send drive commands
```

The wire protocol (3.0) is specified in [docs/protocol-3.md](docs/protocol-3.md);
where this is going is in [docs/roadmap.md](docs/roadmap.md).

## Layout

| Path | What it is |
| --- | --- |
| `crates/tq` | The client SDK. `Session` (connect, subscribe, send on channels, claim locks, clock offsets) and `Publication` (a broadcast that outlives reconnects: tracks, an inbox for channel messages, lock holders, watchdogs). |
| `crates/tq-wire` | Wire format 3.0: control and room messages, group stream and frame headers, datagrams, paths, permission tokens, clock offsets. `web/src/protocol.ts` mirrors it. |
| `crates/tq-transport` | quinn / web-transport-quinn endpoints (BBR, bounded send window), certificates, the per-track group cache (`Track`) and datagram cache, the group sender with its drop policy, and the reader. |
| `crates/tq-media` | Elementary stream handling: NAL framing (Annex B or 4-byte length-prefixed, auto-detected), access unit assembly, SEI capture timestamps, codec strings, parameter set repetition. No decoding. |
| `crates/tq-relay` | Relay binary: one WebTransport endpoint for every session, HTTP for the web app, `/api/cert`, `/api/rooms` and dev tokens. Nothing teleop-specific. |
| `crates/tq-teleop` | Teleop on the SDK: drive and e-stop channels, the dead-man as a lock watchdog, robot status. |
| `crates/tq-publisher` | Robot-side binary: reads the TCP camera sources and publishes them, with teleop, as one broadcast. |
| `crates/tq-tools` | `tq-impair` (Starlink-like UDP impairment proxy) and `tq-probe` (headless console: subscribes to every track and reports per-hop latency; can drive). |
| `crates/tq-app` | Native console (GPUI; VideoToolbox on macOS, NVDEC on Linux). Not built by a plain `cargo build`. |
| `web/` | Browser console in TypeScript, built with bun. `worker.ts` owns WebTransport, WebCodecs and OffscreenCanvas; `main.ts` is the UI. |
| `config/` | Publisher configs: `publisher.example.json` (H.264 + H.265 test sources), `publisher.prodlike.json` (one H.265 camera, 10 fps, 2 Mbps, length-prefixed, like a typical robot camera). |
| `dev/run-local.sh` | Starts the whole stack on one machine. |
| `dev/release.sh` | Bumps a component's version; CI uses it to check tags and write release notes. |
| `build/version.rs` | Build script shared by the binaries: stamps the git commit into `--version` and the handshake. |
| `packaging/` | Publisher tarball/`.deb`/`install.sh` and service files, the relay's Dockerfile, the app's `.dmg` bundler. |
| `infra/gcp/` | Terraform for the relay on GCP (London VM, Let's Encrypt, Cloudflare DNS). The web app deploys to Cloudflare from `web/` with wrangler. |
| `assets/brand/` | The logo (`source.svg`) and `make.py`, which draws the favicon, app icons, GitHub avatar and social preview from it. |
| `docs/` | [protocol-3.md](docs/protocol-3.md), [roadmap.md](docs/roadmap.md), [install.md](docs/install.md). |

## Using the SDK

A program that publishes telemetry and takes commands:

```rust
use tq::wire::{Advertisement, ChannelInfo, DataInfo, Delivery, TrackInfo};

let (rover, mut inbox) = tq::Publication::new(Advertisement {
    broadcast: "rover-7".into(),
    tracks: vec![TrackInfo::data("pose", DataInfo {
        encoding: "json".into(), delivery: Delivery::Latest, rate_hz: Some(50.0), window_ms: None,
    })],
    channels: vec![ChannelInfo {
        id: "goto".into(), label: "Go to".into(), lock: None, encoding: "json".into(), schema: None,
    }],
    ..Default::default()
})?;
let session = tq::Session::connect(options).await?;   // relay URL, room, token, trust
session.publish(&rover).await?;
rover.set_online("pose", true, "");
rover.data("pose").unwrap().send_json(&serde_json::json!({ "x": 1.0, "y": 2.0 }));
while let Some(msg) = inbox.recv().await {
    println!("{} from session {}: {:?}", msg.channel, msg.from_sid, msg.payload);
    msg.accept();   // the sender sees the ack and its round trip
}
```

And one that watches it: `session.subscribe("rover-7/pose".parse()?)` returns a
reader of frames with their timestamps; `session.owner_offset("rover-7")`
puts them on the local clock. `tq-probe` and `crates/tq-relay/src/tests.rs`
are complete examples.

## Versions and releases

The publisher, the relay (with the web app inside) and tq-app are released
separately, each from its own tag (`publisher-v*`, `relay-v*`, `app-v*`).
They only need to agree on the protocol's major version. Every session
exchanges each side's version, commit and protocol. A different minor shows
a warning; a different major is refused. tq-app's status bar and the web
page's header show which app, relay and broadcast owner versions are in
use. See [docs/install.md](docs/install.md).

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
- operator: http://localhost:8080/?room=robot-01&preset=operator
- viewer: http://localhost:8080/?room=robot-01&preset=viewer

Operator controls:
- **Claim** the robot's `drive` lock, then hold `W`/`A`/`S`/`D` to drive (commands at 20 Hz).
- **stream 20 Hz** keeps commands flowing without a key held.
- `Space` releases the lock, so the robot trips immediately. **E-stop** sends on the robot's `estop` channel.

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
cargo run --release -p tq-app -- --preset operator      # or --preset viewer
cargo run --release -p tq-app -- --preset viewer --log-stats   # also log per-track numbers
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

The operator controls match the web page: Claim / Force claim / Release of
the drive lock, hold `W`/`A`/`S`/`D`, and `Space` to release (the robot
trips).

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
./target/release/tq-probe --preset operator --teleop-hz 20 --duration 20   # headless check
```

Tokens: `tq-relay token --preset operator --room robot-01` prints a token;
`--perm sub:robot-01/front` (repeatable) narrows or adds permissions. Pass it
to the page as `?token=`. Without `--dev`, the relay does not mint tokens
over HTTP, and it never mints tokens that may publish.

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

## Protocol (3.0)

The full specification is [docs/protocol-3.md](docs/protocol-3.md). In short:
every session joins a **room** at `/v1/room/{room}` and uses four kinds of
channel.

1. **Control stream** (the first bidirectional stream, length-prefixed
   JSON): `hello`/`welcome`, `advertise`, `subscribe`/`unsubscribe`,
   `claim`/`release`, `keyframe_request`, and `ping`/`pong` for round trips
   and NTP-style clock offsets.
2. **Events stream** (one unidirectional stream each way): the room's
   `state`, then every event in order (`participant_*`, `broadcast_*`,
   `track_*`, `lock_changed`), with fresh states at most every 100 ms. A
   session that falls behind gets a fresh state instead of the events it
   missed.
3. **Group streams**: one unidirectional stream per track per group. For
   video a group is a GOP starting with its keyframe; for data it is one
   message (`latest`) or a window of them (`stream`).
4. **Datagrams**: channel messages into a broadcast, the owner's acks, lock
   heartbeats and their echoes (all routed by the relay to and from the
   right session, after permission and lock checks), and the frames of
   `datagram` data tracks.

The control and events streams are sent at a priority above every group
stream, and tracks are ordered by their own priority, so control never
queues behind video and bulk data never delays it.

### Why one stream per GOP

In an IPPP stream, every frame depends on the one before it. Delivering
frames out of order therefore buys nothing, and per-frame streams would only
add reordering work in the browser.

A stream per GOP gives in-order delivery inside the GOP, and lets a sender
drop a whole stale GOP with a single `RESET_STREAM`. This is the group model
from MoQ.

### Drop policy (`GroupSender`)

| Hop | Old group when a new one starts | Late frame |
| --- | --- | --- |
| owner → relay | reset immediately | dropped once older than `max_lag_ms` (400); skip to next keyframe |
| relay → `realtime` subscription (default) | reset immediately | `--realtime-max-lag-ms` (250) |
| relay → `smooth` subscription | `--smooth-keep-groups` (1) older groups may finish | `--smooth-max-lag-ms` (1500) |

"Late" is measured from when the frame arrived on this node. Writes are
bounded by the QUIC `send_window` (1 MiB). Once it is full, a frame can't
sit inside QUIC for seconds, so it blocks, ages past the limit and gets
dropped.

On the browser side, a group stream that is superseded is cancelled. A
decoder that has held a frame for more than 150 ms skips to the next
keyframe.

There is no encoder control hook: the encoder is a separate process. A skip
therefore recovers at the encoder's periodic IDR (1 s in gstreamer-source).
Keyframe requests are still sent all the way to the owner, which logs
them, so an encoder hook can be added there. A track nobody subscribes to
is not sent at all.

### Teleop and the dead-man switch

Teleop is built from general parts (`crates/tq-teleop`): the robot's
broadcast has a `drive` channel behind a `drive` lock, an open `estop`
channel, and `.status` / `.events` data tracks. The relay only lets the
lock's holder send on `drive` or heartbeat the lock. Commands are
latest-wins by `seq`. The lock's watchdog, run by the robot, keeps it
safe-stopped (tripped) whenever any of these is true:
- nobody holds the lock, or the holder changes;
- no heartbeat has arrived for `heartbeat_loss_ms` (500);
- commands have started and then stopped for `command_stale_ms` (500);
- the relay connection drops.

It stays tripped until the next accepted command. `LoggingSink` stands in
for the robot interface (`tq_teleop::Sink`).

## Measuring

Both the page overlay and `tq-probe` report latency per hop. Each line is
`avg/max` over the last 0.5 s.

| Line | Measured from → to | Clock(s) |
| --- | --- | --- |
| `capture→pub` | SEI capture timestamp → the owner has the whole access unit. Covers the camera pipeline after the driver timestamp, encode, TCP and AU assembly. | robot only |
| `pub→relay` | owner → relay received the frame (the uplink) | robot ↔ relay offset |
| `relay→browser` | relay → the frame's last byte read in the worker | relay ↔ browser offset |
| `decode` | `VideoDecoder.decode()` → output callback | browser only, exact |
| `capture→drawn` | capture → `drawImage` returned | both offsets |
| `draw→display≈` | `drawImage` → estimated scan-out: the next animation frame plus one refresh interval. Panel response time is not included. | browser only, estimate |
| `glass→glass≈` | capture → estimated scan-out, plus `?sensor_ms=` | both offsets |

The `±… sync` figure is the worst-case clock error. The relay pings every
session, and every session pings the relay, every 500 ms. Each side keeps
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

Results from 2026-10-06 on one MacBook, protocol 2.0: VideoToolbox test sources, links
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
  relay serves rooms by id from the URL.
- The consoles drive only the first broadcast in a room that has a `drive`
  lock, and show only video tracks plus `.status`/`.events`; data widgets
  come with the layout spec (roadmap phase 3).
- Relay HTTP is plain HTTP. Remote browsers need HTTPS in front of it, or
  a real certificate.
