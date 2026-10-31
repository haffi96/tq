# Roadmap: from teleop to a general low-latency stack

tq becomes **tele over QUIC**: low-latency video, telemetry and control
between machines and people, over QUIC/WebTransport, with latency measured
end to end on every frame. Robotics is the focus. Teleoperation becomes the
first fully supported example, built from the same parts anyone else uses.

Nobody depends on the current releases, so nothing here keeps backwards
compatibility. The protocol goes to 3.0 ([protocol-3.md](protocol-3.md)).

## Decisions

| Question | Decision |
| --- | --- |
| Name | Keep `tq`, now standing for *tele over QUIC*. |
| Protocol | Our own wire format, using MoQ terms (room, track, group, frame). It is "MoQ-style", not IETF MoQT. A MoQT/moq-lite mapping can come later. |
| Core | Rust, and nothing teleop-specific in it. `grep -ri teleop crates/tq-relay crates/tq-wire crates/tq-transport` returns nothing. |
| Latency | Per-hop timestamps on every frame (video and data). SEI capture times stay. Per-hop clock offsets (NTP-style) let any participant put every timestamp on its own clock. |
| UI | One layout spec (JSON) that both apps render. Web leads the spec and editor. Native (GPUI) is the high-performance console, and gets real investment. |
| SDKs | Rust first and uncompromised. TS and Python are possible, with small examples, but not a priority. |
| Sources | A `Source` trait and a registry, so adding a source is one module. TCP Annex B stays and doubles as an out-of-process plugin boundary. |

## Concept mapping

| Today | 3.0 |
| --- | --- |
| publisher / operator / viewer | **sessions**: any session may publish, subscribe or both, as its token allows |
| a publisher's cameras | a **broadcast**: a named bundle of tracks, channels and locks that a session advertises |
| camera (`camera: u16`) | **track** with a kind (`video` or `data`) and a priority |
| `Role` in the token | **permissions** with path patterns (`sub:rover-7/telemetry`, `write:rover-7/drive`, `pub:alice`). Operator, viewer and robot remain only as presets for dev tokens |
| every client gets every camera | explicit **subscribe**; owners can see which tracks have subscribers |
| teleop datagrams | **channels**: named message lanes into a broadcast, with seq, timestamps, ack and echo built in |
| operator claim | **locks**: a named exclusive lock over some of a broadcast's channels |
| dead-man | **watchdog** on a lock, run by the owner's SDK |
| `TeleopStatus`, dead-man events | `.status` and `.events` **data tracks** |

## Current state (handoff)

Updated 2026-10-31 (evening). Branch `generalize-core`, open as PR #9
against `main`, up to date with it. Every CodeRabbit thread is answered
and resolved, through its review of `0c49a27` (web layouts). CodeRabbit
then paused automatic reviews on the PR (too many commits), so the tq-app
commit `f2823f2` and the review fixes `3b02fea` have had no CodeRabbit
review: comment `@coderabbitai review` (or `resume`) to get one.
The tq-app layout editor is on branch `tq-app-editor`, stacked on
`generalize-core` (its own PR); neither is merged.
Phase 1 is done; Phase 2's Rust half is done, tq-edge and the "about 10
lines" SDK goal included. Phase 3: both consoles render layouts
([layout-1.md](layout-1.md)) and edit them (see below). Start with [protocol-3.md](protocol-3.md) for the wire
format, `crates/tq/src/lib.rs` for the SDK, `crates/tq-relay/src/tests.rs`
for worked examples of every feature, and [install.md](install.md#the-config)
for tq-edge's config.

**Where things are:**
- `crates/tq`: the SDK. `Session` (one connection), `Client` (reconnects,
  resubscribes, republishes), `Session::hold` (claim + heartbeat a lock),
  `Frame::hops()` / `capture_local_us()` (timestamps on the local clock),
  `tq::discover` (relay info, dev tokens, `Options::discover`). The short
  path: `Publication::builder`, `Publication::video` (a `VideoWriter`),
  `Token::publisher`, `Client::discover`; `examples/camera.rs` uses them.
- `crates/tq-teleop`: drive channel behind a dead-man lock, the remote
  stop (latching), `.status` / `.events`.
- `crates/tq-edge`: the device agent (was tq-publisher). Config-driven:
  tracks fed by sources (`src/source/`, `tcp` today, video through the
  SDK's `VideoWriter`), channels feeding sinks (`src/sink.rs`: `log`,
  `tcp`), optional teleop with its own sink.
- Consoles: `web/` and `crates/tq-app` (GPUI), both on protocol 3.0.
  The web console is a board of widgets: `web/src/layout.ts` (spec,
  presets, placement), `catalog.ts` (what can be bound, fields seen,
  permissions), `widgets.ts` (the types), `board.ts` (rendering,
  subscriptions, the editor). tq-app renders the same layouts:
  `crates/tq-app/src/layout.rs` (spec, preset, resolving; tests check the
  preset matches the web's) and `src/ui/board.rs` (widgets as cached
  views; the session subscribes to what they want, keeps data per track
  and holds locks per lock; in edit mode, overlays to drag, resize and
  remove; one column under 700 px). `src/ui/editor.rs` is the editor
  panel (catalog, bindings, props, warnings); `layout.rs` also has the
  widget types (`DEFS`, checked against the web's), `settle`, `stacked`
  and `Store` (where layouts are saved). `--layout` or the settings file
  picks one, else the saved one; `config/layout.example.json` is an
  example.
- `dev/fake-device.ts`: stand-in device programs for
  `config/edge.full.example.json` (telemetry that follows the drive
  commands, the lights sink, the teleop sink); `dev/run-local.sh` starts
  it with that config.

**Verified (all on the current branch head):**
- `cargo test` and `cargo clippy --all-targets`: clean, including the
  relay's end-to-end tests (16), tq-edge's config tests and the SDK's.
- `cargo build -p tq-app && cargo clippy -p tq-app --all-targets`: clean.
  tq-app is not a default member, so plain `cargo build` skips it.
- `cd web && bun run typecheck && bun run build`: passes.
- Live against `dev/run-local.sh`: both consoles (video on H.264 and
  H.265, claim, WASD and 20 Hz streaming, stale-message trip, remote stop
  from another operator, re-arm, release, force-claim both ways); a relay
  restart under tq-edge and tq-app (both reconnect, video resumes, the
  lock comes back free); tq-edge's full example with a stand-in device
  program (data source, channel sink, teleop sink, refusals while the
  program is gone); `examples/camera.rs` fed by ffmpeg alongside tq-edge
  (about 30 fps, its channel message acked) with the regression check
  still passing.

- Phase 3 web, live against `CONFIG=config/edge.full.example.json
  dev/run-local.sh` in headless Chrome (Playwright): the operator and
  viewer presets; claim, WASD driving (acked, the device's speed follows),
  Space STOP (acked, lock let go, robot latched); adding a plot, a number
  and a toggle in the editor and binding them from the catalog (the toggle
  switched the device's lights and shows their state from telemetry);
  dragging with push-down and float-up; a 420 px wide window; tq-probe's
  regression check alongside.
- Phase 3 native, live with the same stack: tq-app on the preset and on
  `config/layout.example.json`; claim, WASD (acked, speed follows, the
  plot draws it), Space STOP (acked, lock let go, latched); the lights
  toggle (acked, device's state read back); a bad layout file refused with
  every problem listed. Moving the video tiles out of a cached view took
  decoded→painted from about 9 ms to about 6 ms.
- The CodeRabbit fixes (`3b02fea`), live: drive controls disabled until
  the lock is held, on both consoles; a web widget made to throw shows
  `draw failed: …` while the rest keep going; the web page starts (STOP
  included) with `localStorage` throwing; a viewer token with
  `--perm 'write:*/stop'` gets STOP and its Space stop is acked, a plain
  viewer gets none.

- tq-app's editor (branch `tq-app-editor`), live against
  `CONFIG=config/edge.full.example.json dev/run-local.sh`: overlays over
  every widget with both videos still playing underneath; dragging a plot
  onto a pinned lock (it settles under it, the rest make room; the file
  is rewritten); rebinding the plot's field from the fields seen and
  clearing Min/Max (Apply saves `{"path": "battery_v"}`, bare ids kept);
  Add (bound to the first video, the duplicate-video warning shows),
  Remove; the preset edited, saved under the settings directory, loaded
  again after a restart, and Reset removing the file; narrow stacking
  (with a raised threshold); a non-number refused on Apply; `w` typed in
  a field does not drive, Space in a field is still STOP (lock let go,
  robot latched); tq-probe's regression check alongside.

**Regression check** (needs `../streaming-tools` next to this repo, or
`STREAMING_TOOLS=…`): run `dev/run-local.sh` (or
`CONFIG=config/edge.full.example.json dev/run-local.sh` with programs on
:6000–6002), then
`TQ_SHARED_SECRET=dev-secret-change-me ./target/release/tq-probe --room robot-01 --preset operator --teleop-hz 20 --duration 8 --json`
(add `--send 'robot-01/lights={"on":true}'` to try a channel). Expect
both video tracks at about 30 fps with `capture_to_probe_ms` p50 in the
tens of ms, every drive command acked `Ok`, and `deadman_tripped: false`.
Then open `http://localhost:8080/?room=robot-01&preset=operator` and run
`./target/release/tq-app --preset operator`.

**Remote stop (decided 2026-10-31, safety-critical):** a remote stop must
stop the robot under all circumstances. Both consoles' STOP (button and
Space) send on every robot's `stop` channel, resend every 50 ms until
acked (2 s, then the log says no answer), and let go of their own drive
locks (tq-app: every lock it holds). Space is taken before any focused
control can act on it. STOP is not a widget: it is in each console's
header, outside any layout. It is offered to the operator preset and to
any session allowed to write a room's `stop` channel, and once offered it
stays. A drive widget sends only while its lock is held, and losing the
lock turns streaming off. The robot
latches: it rejects drive commands until its drive lock is claimed again
at least 500 ms later (Claim while holding re-claims); a robot reconnect
does not re-arm it (claims are numbered). Release lets go without
stopping. It is a remote stop, not an E-stop: in the field an E-stop is a
physical button on its own safety-rated link, which this does not
replace. Keep all of this when touching the stop, the drive lock or
console key handling.

**Known gaps and notes:**
- tq-app has no gamepad input, and does not unsubscribe widgets
  scrolled out of view. Its editor's binding pickers are built when a
  widget is selected: a track that appears later needs the widget
  selected again.
- Widgets on the same channel share its last ack line.
- Web: Space is STOP even in the editor's text fields (paste text with
  spaces). Gamepad drive is in but untested with a real pad (the user
  will test it). No `map` widget yet. Layouts live in `localStorage`
  only. A widget that throws stays marked failed until it is remade
  (edited, or the layout changes).
- A killed relay does not close connections, so clients notice only at
  the QUIC idle timeout.
- tq-app's "Held by" badges show a bare session id.
- Relay replay of `.events` on subscribe is by design; both consoles now
  log those lines at the time they happened.
- Testing tips: tq-app's window cannot be resized from AppleScript or by
  dragging its edge with CGEvent; to see narrow mode, build once with
  `NARROW` in `ui/board.rs` raised. Run it with `HOME=/tmp/…` to test the
  per-room layout store without touching the real settings directory.
  On this machine the T3 Code app probes every new local
  listening port with an HTTP `GET /`. Raw TCP test servers must tolerate
  stray connections. Drive tq-app's GUI with osascript System Events
  keystrokes (CGEvent key events do not reach it; CGEvent clicks do: a
  ten-line Swift program posting `leftMouseDown`/`Up` works; screen
  points are screenshot pixels / 2). The web console runs in headless
  Chrome through `playwright-core` with the installed Google Chrome
  (WebTransport and HEVC work); keep that script outside the repo.
  `tq-relay --secret … token --preset viewer --perm 'write:*/stop'` mints
  tokens with extra permissions.

**Next:** layouts per
room on the relay so both consoles share them, a `map` widget. The TS client
package, golden test vectors and a pyo3 example are lower priority and
can come whenever.

## Phases

Every phase ends with teleop working end to end (`dev/run-local.sh`, both
apps, tq-probe). Teleop is the regression test.

### Phase 1: generic core (protocol 3.0) — done

- `tq-proto` → `tq-wire`, implementing [protocol-3.md](protocol-3.md):
  one endpoint for all sessions, broadcasts, tracks with kinds and
  priorities, explicit subscriptions (with `max_rate_hz` and a drop
  policy), channels, locks, path permissions, datagram tracks.
- Relay: rewrites broadcast indices between owner and room, routes channel
  datagrams by sender session, enforces locks and permissions, measures
  track stats itself, knows nothing about teleop. Many broadcasts per room.
- `tq-teleop`: drive and remote stop channels, the dead-man as a lock watchdog,
  robot status on `.status`.
- Data tracks: any session can publish them, with the same latency trace as
  video. Device state and events are `.status` and `.events` tracks.
- Clocks: `tq_wire::Offset` composes the per-hop offsets with their error
  bounds; `Session::owner_offset` gives any owner's clock.
- Web, tq-app, tq-probe and tq-edge speak 3.0.
- Checked: the grep above is empty; relay end-to-end tests cover
  publishing, subscribing, permissions, thinning, channels, locks, the
  watchdog, outages and takeover; teleop works end to end with the real
  encoders through tq-probe and both consoles.

### Phase 2: SDK extraction — Rust half done

Done (the crate was pulled into Phase 1 so each client was ported once):
- A `tq` crate: `Session` (subscribe, channels, locks, clocks) and
  `Publication` (a broadcast that outlives reconnects: tracks, an inbox for
  channel messages with latest-wins sequencing, lock holders, watchdogs).
  tq-edge, tq-probe and tq-app use it.
- The gaps found porting tq-app:
  - `Client`: reconnects with backoff (longer after an incompatible
    relay), resubscribes (`Client::subscribe` readers survive reconnects,
    with group numbers that keep increasing), publishes again, and carries
    the room, events and replies across sessions. Locks do not carry over.
  - `Session::hold(lock, force, interval)`: claims and heartbeats; `lost()`
    when forced away or the session ends; dropping it releases.
  - `Frame::clocks`, `capture_local_us()` and `hops()` put a frame on the
    local clock; `Session::wait_owner_offset` covers frames that arrive
    before the first ping.
  - `tq::discover`: `/api/info`, dev tokens, a `Token` (given, signed or
    dev) and `Options::discover`, over HTTP or HTTPS.
  - tq-app runs on `Client`, `hold` and `discover`; tq-edge on
    `Client`; tq-probe on `hold` and `hops`. Tests: `hold_heartbeats_*`,
    `client_resubscribes_and_republishes`, `discover_over_http`. Checked
    live, including a relay restart under both.

- `tq-publisher` → `tq-edge`, a config-driven agent on the device: tracks
  fed by sources, channels feeding sinks, teleop optional
  (`config/edge*.json`, [install.md](install.md#the-config)). Sinks: `log`,
  and `tcp`, which writes JSON lines to the device's own program and
  refuses messages while that program is not connected. Teleop's sink
  gets drive commands and safe-stops, and a safe-stop on every connect.
  Packaging, release workflow and docs renamed with it. Checked live with
  `config/edge.full.example.json` (a data source and both sinks from a
  stand-in program, including refusing while it is gone) and the plain
  example with both consoles.

- Publishing a camera and a channel is about 10 lines of Rust
  (`crates/tq/examples/camera.rs`, H.264/H.265 on stdin):
  `Publication::builder(name).video(id).channel(id).build()`,
  `Token::publisher(secret, name)` (publish tokens are never minted over
  HTTP), `Client::discover(url, room, token)`, and `Publication::video`,
  a `VideoWriter` that takes an encoder's byte stream (`write`) or whole
  access units (`write_frame`, out at once; `write_frame_at` with the
  camera's capture time, which sets `CAPTURE_FROM_SEI` like an SEI time
  does). tq-edge's TCP video source runs on it. Test:
  `video_writer_publishes_an_encoder_stream`.

Still to do:
- TS client package taken out of `web/src/worker.ts` and `protocol.ts`.
- Golden test vectors (encoded bytes and decoded values) in the repo, checked
  by Rust and TS alike, so the two codecs cannot drift apart.
- Python: one small example through pyo3, nothing more yet.

On FFI: it helps with native languages (Python via pyo3, C/C++ via a C ABI
made with cbindgen, Go and Swift through the C ABI). In those, the Rust SDK
does the work and a binding is thin. It does not help in the browser: the
browser must use its own WebTransport and WebCodecs, so the TS client stays
a separate, small implementation of the wire format. The test vectors keep
it honest. WASM could share the codec, but the codec is the easy part.

### Phase 3: widgets

A **layout** is a JSON document: a grid of widgets, each bound to data by
path. Both apps render it.

```json
{
  "version": 1,
  "grid": { "cols": 12, "row_height": 40 },
  "widgets": [
    { "id": "w1", "type": "video", "at": [0, 0, 8, 9], "bind": { "track": "front" } },
    { "id": "w2", "type": "plot", "at": [8, 0, 4, 4], "bind": { "track": "telemetry", "path": "battery.voltage" } },
    { "id": "w3", "type": "drive", "at": [8, 4, 4, 5], "bind": { "channel": "drive", "lock": "drive" }, "props": { "rate_hz": 20 } }
  ]
}
```

- **Data catalog.** The app lists everything in the room: tracks (and, for
  data tracks, the fields seen so far), channels, locks, device state and
  events. Binding a widget means choosing from the catalog.
- **Editing.** Add, remove, drag and resize widgets; bind them from the
  catalog; save the layout (locally first, later per room on the relay).
  Some things are locked: a layout can pin widgets, and permissions decide
  which channels and locks a user can bind.
- **Performance first.** Each widget type declares its cost (its decode
  work, redraws per second, and whether it sits on the path from frame
  arrival to screen). Video and latency-critical widgets have a protected
  path: no other widget can delay a frame being drawn. Widgets that could
  slow it (dense plots on fast data, many video tiles at full rate) can
  still be added, with a visible warning that says what they cost. The app
  measures its own time to present and shows it, as tq-app's status bar
  does today.
- **Ingest rate is not render rate.** Data is consumed at its arrival rate
  (kHz is fine) and drawn at the display's rate: numbers show the newest
  value, plots append to a ring buffer and redraw once per display frame.
  A widget that needs fewer samples than the track carries subscribes with
  `max_rate_hz`, so the relay thins the data before it is sent.
- **Catalog v1:** video (with latency HUD), latency per hop, number, plot,
  button, toggle, slider, keyboard/gamepad drive, lock control, event log,
  status, map.
- **Teleop preset:** video grid, drive, lock control, remote stop, latency.
- **Done on the web:** the spec ([layout-1.md](layout-1.md)), the catalog
  (tracks with fields seen, channels, locks, filtered by permissions), the
  editor (add, remove, drag, resize, bind, props, pin, import/export,
  saved per room in the browser), presets made from the room, subscriptions
  that follow visible widgets with merged `max_rate_hz`, batched ingest
  drawn once per frame, cost warnings and the page's own draw time, and
  every catalog v1 widget except `map`.
- **Done in tq-app:** rendering layout 1 (every widget but `map`), the
  same preset, `--layout`, subscriptions following the widgets, and the
  editor (as the web's, in the dock; saved per room and preset in the
  settings directory, or back to the `--layout` file), with one-column
  stacking under 700 px.
- **Order:** web first to settle the spec and the editor. Then tq-app renders
  the same spec with GPUI, keeping the rules that keep video fast (cached
  views, video surfaces redrawn on their own). After that, the editor comes
  to native too.

### Phase 4: sources

- Done with tq-edge: a source is a module in `crates/tq-edge/src/source/`
  picked by `"type"` in the config. `tcp` feeds video tracks (encoder byte
  streams) and data tracks (JSON lines or length-prefixed messages), so
  any process that writes to a socket is a source with no Rust involved:
  ```json
  { "id": "front", "kind": "video", "source": { "type": "tcp", "addr": "127.0.0.1:5010" } }
  ```
- To do: `rtsp` (IP cameras), `stdin` and `unix` sockets; Rust sources
  behind cargo features.
- Later: a GStreamer sink element and a ROS 2 bridge (images to video
  tracks, topics to data tracks, channels to topics).

### Phase 5: positioning

- Done: README and docs describe tq as tele over QUIC, with teleop as the
  first example.
- `examples/`: teleop, multi-camera monitoring, PTZ inspection, a fleet
  dashboard.
- Repo and package names follow if anything changes.

## Out of scope for now, planned next

- Transcoding and adaptive bitrate (frames pass through untouched).
- Audio.
- Direct paths: a relay on the robot or LAN for local users, relays
  chained to the cloud one, then peer-to-peer QUIC.
- Reliable channel mode (3.1).
- Room state filtered per session; rate and bandwidth limits per
  permission; EdDSA tokens.
- Auth beyond signed tokens (OIDC, per-user permission management).
- A MoQT / moq-lite mapping on the relay.
