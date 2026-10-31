# tq protocol 3.0

Status: draft for Phase 1 of the [roadmap](roadmap.md). It replaces 2.0
(`crates/tq-proto`, now `crates/tq-wire`) and does not interoperate with it.

3.0 keeps what already works in 2.0: one WebTransport session per hop,
groups on their own streams with newest first, per-hop timestamps, the
events stream, and version checks. It removes the teleop concepts from the
wire and makes every session the same kind of thing: any session can
publish, subscribe, or both.

All integers are big endian. Times are Unix microseconds.

## Terms

| Term | Meaning |
| --- | --- |
| **room** | A namespace on the relay. Everything below lives in one. |
| **session** | One WebTransport connection to the relay. A robot, a browser and tq-app are all sessions. What a session may do comes from its token, not from its kind. |
| **broadcast** | A named bundle a session publishes: tracks, channels and locks. A robot publishes `rover-7`; an operator's console might publish `alice` with a gamepad track. Named `<room>/<broadcast>`. |
| **track** | One stream inside a broadcast: `video` or `data`. Addressed as `<broadcast>/<track>`. |
| **channel** | A message lane *into* a broadcast: other sessions send, the broadcast's owner receives. Commands, set-points, buttons. |
| **lock** | Makes some of a broadcast's channels exclusive to one session at a time. |

Tracks carry data out of a broadcast to any number of subscribers. Channels
carry small messages into a broadcast from whoever may write. A teleop robot
publishes video and telemetry tracks and owns a `drive` channel behind a
`drive` lock.

## Sessions

```text
 session ==WT /v1/room/{room}?token=…==> relay <==WT== session
```

- There is one endpoint. The token decides what the session may publish
  and subscribe to.
- Each session uses four kinds of channel, as in 2.0:
  - **Control stream**: the first bidirectional stream, opened by the
    session. Length-prefixed JSON (`len:u32` and an object with a `type`).
  - **Events stream**: unidirectional. Relay → session: the room's state and
    events. Session → relay: edges of its own broadcast (tracks going
    online or offline).
  - **Group streams**: unidirectional, one per track per group, in either
    direction.
  - **Datagrams**: channel messages, lock heartbeats, acks and echoes.

## Handshake

The first control message is `hello {build}`. The relay answers:

```text
welcome { session: "k3f9…", sid: 7, perms: [...], relay: BuildInfo }
```

`sid` is a `u32` the relay assigns to each session in a room. Datagrams use
it for routing. `perms` are the session's effective permissions, so apps can
hide what a user cannot do. Different protocol majors close with
`UNSUPPORTED_VERSION`. A different minor logs a warning. The `hello` and
`welcome` build fields and that close code keep their shape across majors.

## Permissions

Tokens are HS256 JWTs signed with the relay's shared secret, as in 2.0:

```json
{ "sub": "alice", "room": "site-a", "exp": 1767225600,
  "perms": ["sub:rover-7/*", "write:rover-7/drive", "lock:rover-7/drive", "pub:alice"] }
```

A permission is `verb:pattern`. Patterns are paths whose segments may be `*`.

| Verb | Pattern | Allows |
| --- | --- | --- |
| `pub` | `<broadcast>` | advertising a broadcast with that name (and replacing an older one) |
| `sub` | `<broadcast>/<track>` | subscribing to a track |
| `write` | `<broadcast>/<channel>` | sending on a channel |
| `lock` | `<broadcast>/<lock>` | claiming a lock while it is free |
| `force` | `<broadcast>/<lock>` | taking a lock from its holder |

So a token can be as narrow as `sub:rover-7/telemetry`: one data track and
nothing else. Every session with a valid token for the room gets the room
state and room events (who is there, which broadcasts and tracks exist,
who holds which lock).

Presets for dev tokens: **operator** = `sub:*/* write:*/* lock:*/* force:*/*`,
**viewer** = `sub:*/*`, **robot** = `pub:<name>`.

Later minors may add: room state filtered per session, so unreadable tracks
are hidden instead of refused; rate and bandwidth limits per permission;
and asymmetric signatures (EdDSA), so services can verify tokens without
the secret.

## Advertise

A session publishes a broadcast by advertising it, and advertises again
whenever something changes. A session may advertise several broadcasts. A
3.0 relay may limit this to one per session (see
[Multiple broadcasts](#multiple-broadcasts)).

```json
{ "type": "advertise", "broadcast": "rover-7",
  "tracks": [
    { "id": "front", "kind": "video", "label": "Front",
      "video": { "codec": "h265", "width": 1280, "height": 720, "fps": 30 } },
    { "id": "telemetry", "kind": "data", "label": "Telemetry",
      "data": { "encoding": "json", "delivery": "latest", "rate_hz": 20 } },
    { "id": ".status", "kind": "data", "data": { "encoding": "json", "delivery": "latest" } },
    { "id": ".events", "kind": "data", "data": { "encoding": "json", "delivery": "stream" } }
  ],
  "channels": [
    { "id": "drive", "label": "Drive", "lock": "drive", "encoding": "json" },
    { "id": "estop", "label": "E-stop", "encoding": "json" }
  ],
  "locks": [
    { "id": "drive", "label": "Drive",
      "watchdog": { "heartbeat_loss_ms": 500, "message_stale_ms": 500 } }
  ] }
```

- The relay answers `advertise_ok {broadcast, relay_us}` or
  `advertise_error {broadcast, reason}`. The relay diffs each new
  advertisement against the previous one and emits `track_published`,
  `track_updated` and `track_unpublished` events.
- A newer session advertising the same broadcast name replaces the older
  one, whose session closes with `REPLACED` if that was its only broadcast.
  This is how a robot that reconnects takes its place back.
- `watchdog` is informational. The owner enforces it, and apps can show it.
- Track `meta` and channel `schema` are optional free-form JSON for apps
  (units, ranges, field descriptions). The relay passes them through.
- Track ids that start with `.` are conventions the SDK fills in:
  - `.status`: low-rate device state (mode, battery, faults).
  - `.events`: anything the owner wants in an event log, plus its watchdog
    edges (`watchdog_tripped`, `watchdog_cleared`).

  They are ordinary data tracks, so the same permissions apply (a token can
  allow `sub:rover-7/.status` and nothing else).

### Indices

Group streams and datagrams name tracks, channels and locks by `u16`
indices instead of strings.

- **Owner ↔ relay:** an index is the position in the owner's latest
  advertisement of that broadcast, plus a `u16` broadcast index: the
  position of that broadcast among the session's advertised broadcasts.
- **Relay ↔ subscriber:** an index is the room-wide index the relay
  assigned, listed in the room state.

The relay rewrites indices as it forwards. Room indices stay the same for
as long as the track, channel or lock exists in the room.

## Subscribing

A session gets only the tracks it asks for:

```text
subscribe   { tracks: ["rover-7/front", { "track": "rover-7/imu", "max_rate_hz": 60 }] }   (adds)
unsubscribe { tracks: ["rover-7/front"] }
subscribe_result { track, ok, reason }
```

- Each track needs `sub:`. Subscriptions persist when a track goes offline
  and comes back.
- A subscription to a track that does not exist yet waits for it.
- The relay forwards a track from its owner only while someone subscribes,
  and tells the owner with `track_demand {track, subscribers}`. An owner may
  stop encoding idle tracks.
- Apps subscribe to what their widgets are bound to. A hidden widget
  unsubscribes, so no bandwidth goes to it.

## Group streams

```text
group stream := type:u8 (=0x01) version:u8 (=2) track:u16 group:u64
                codec_len:u8 codec:utf8[codec_len]
                frame*
frame        := len:u32 index:u32 flags:u8 capture_us:u64 publisher_us:u64
                relay_us:u64 frame_id:u32 payload
```

This is 2.0's format with `camera` renamed to `track` and the version bumped
to 2. On the owner → relay hop, `track` is preceded by `broadcast:u16`. Flags:
bit 0 `KEY`, bit 1 `CAPTURE_FROM_SEI`. A frame holds at most 16 MiB.

**Video tracks.** A group is a GOP: it starts with a keyframe carrying its
parameter sets, and the payload is one Annex B access unit. `codec` is the
WebCodecs string. Same as 2.0.

**Data tracks.** `codec` is the encoding (`json`, `cbor` or `bytes`). Each
frame is one message. `delivery` decides the grouping:

- `latest`: each message is its own group. Newer messages supersede older
  ones, and senders drop stale ones as they drop old GOPs. The relay keeps
  the latest group, so a new subscriber gets the current value at once.
  For state: pose, battery, speeds.
- `stream`: a group holds all messages in a window (default 1 s), and
  receivers get all of them, in order, unless they fall further behind than
  the lag limit. For logs, detections, point clouds, bulk transfers.
- `datagram`: each message is a QUIC datagram (see
  [High-rate data](#high-rate-data)). No streams at all. For small messages
  at high rates: IMU, joint states, odometry, force-torque.

Data frames carry the same timestamps as video, so every telemetry value has
a measured age when shown.

### High-rate data

At 100 Hz to a few kHz, the cost is the number of messages, not the bytes:
1 kHz of 100-byte samples is under 1 Mbps. So the delivery mode matters
more than any limit.

| Rate | Small messages (< 1 KB) | Larger messages |
| --- | --- | --- |
| up to ~50 Hz | `latest` | `latest` |
| 50 Hz – kHz, newest matters most | `datagram` | `stream` |
| 50 Hz – kHz, every sample matters | `stream`, optionally batched | `stream` |

- `latest` opens one QUIC stream per message, which is cheap at telemetry
  rates but wasteful at hundreds per second.
- `datagram` costs one datagram per message and never waits on a lost
  packet. Lost samples are simply missing, which is what you want for "the
  arm is here now".
- `stream` delivers every sample in order on one stream per window. A lost
  packet holds the samples behind it for about one round trip.
- **Batching** (SDK option, `stream` only): the owner packs samples that
  arrive within `batch_ms` into one frame, each with its own time offset.
  This cuts per-frame overhead at kHz rates, at the cost of up to
  `batch_ms` of extra latency. Off by default.

Datagram track frames:

```text
0x24 track_frame  track:u16 seq:u32 flags:u8 capture_us:u64 publisher_us:u64
                  relay_us:u64 payload
```

On the owner hop, `track` is preceded by `broadcast:u16`. The whole datagram
must fit in 1100 bytes. Datagrams may arrive out of order. A receiver
showing the newest value drops any `seq` older than the last one it saw,
and a receiver plotting inserts by `capture_us`.

**Who throttles.** The owner publishes at the source's natural rate; the
protocol sets no maximum. Rates come down in three places:

1. **Per subscription, on the relay.** `subscribe` takes an optional
   `max_rate_hz` per track. For `latest` and `datagram` tracks the relay
   forwards at most that rate to that subscriber, always the newest
   message (one timestamp compare per message). `stream` tracks are never
   thinned: a subscriber that wants every sample gets every sample.
2. **Congestion, on every sender.** A subscriber on a slow link falls
   behind. `latest` and `datagram` tracks drop old messages; `stream`
   tracks reset groups older than the lag limit, as video does. A slow
   subscriber never slows the owner or other subscribers.
3. **Rendering, in the apps.** Messages are consumed at their arrival rate
   and drawn at the display's rate. A number widget shows the newest
   value; a plot appends every sample to a ring buffer and redraws once per
   display frame. No widget redraws per message.

The relay may still cap rates and bandwidth per track or per permission, to
protect itself. That is planned for a minor, and it is not needed for
correctness.

### Priority

Each track has a `priority` (0–255, higher first; default 128 for video,
96 for data). Senders and the relay order streams by track priority first,
then newest group first. So a bulk data track can use what bandwidth is left
without delaying video or control.

## Data between sessions

Any session can publish, so a stream between clients is a broadcast like
any other. The console publishes `alice` with a `gamepad` data track, and
the robot (or another console) subscribes to `alice/gamepad`. What you get:

- **Throughput:** tracks travel on QUIC streams under the connection's
  congestion control, with no datagram size limit (frames up to 16 MiB).
- **Latency:** the same per-hop timestamps and drop policy as video.
- **Isolation:** priority keeps bulk transfers from delaying video.

Channels remain the right tool for small, latency-critical messages that
need acks and locks. Tracks are for everything else that flows.

Every path still goes through the relay. Direct paths that bypass it are a
later addition (see the roadmap): a relay on the robot or the LAN for local
users, then peer-to-peer QUIC.

## Channels and datagrams

```text
subscriber <-> relay: type:u8      body
relay <-> owner:      type:u8 sid:u32 body       (sid = the sending session)

0x20 message    id:u16 (channel) seq:u32 sent_us:u64 payload
0x21 ack        id:u16 (channel) seq:u32 echo_us:u64 status:u8 detail:utf8
0x22 heartbeat  id:u16 (lock)    seq:u32 sent_us:u64
0x23 echo       id:u16 (lock)    seq:u32 echo_us:u64 flags:u8
```

Type `0x24` is a datagram track frame; see [High-rate data](#high-rate-data).
It travels owner → relay → subscribers like any track, so it carries no
`sid`.

- `id` follows the index rules above. On the owner hop it is preceded by
  `broadcast:u16`.
- The relay adds the sender's `sid` on the way to the owner, and the owner
  puts the target's `sid` on acks and echoes. The relay strips `sid` and
  forwards to that session only.
- **Relay checks:** a message needs `write:` on the channel, and, if the
  channel belongs to a lock, the sender must hold it. A heartbeat needs the
  sender to hold the lock. Anything else is dropped and counted; it never
  reaches the owner.
- **Sequencing:** latest wins per channel and sender. A `seq` at or below
  the last one applied is acked `superseded` and not applied. The sequence
  restarts when a lock's holder changes.
- **Ack status:** `0 ok`, `1 superseded`, `2 rejected`, `3 malformed`.
- **Echo flags:** bit 0 `TRIPPED`, meaning the lock's watchdog has tripped.
- `sent_us` and `echo_us` are on the sender's clock and only compared with
  themselves, so round-trip time needs no clock sync.
- The whole datagram must fit in one QUIC datagram: senders keep it at or
  below 1100 bytes.

Lost datagrams are fine for commands repeated at a fixed rate. A reliable
mode for one-off messages (on the control stream, with the same checks and
acks) is planned for 3.1 as `"mode": "reliable"` on the channel.

## Locks

```text
session -> relay: claim   { lock: "rover-7/drive", force }
                  release { lock }
relay -> session: claim_result { lock, ok, holder, reason }
relay -> owner:   lock_holder  { broadcast, lock, sid }        (sid null = free)
event:            lock_changed { lock, from, to, reason }
                  reason: claimed | forced | released | left
```

The relay owns locks. A holder's session ending frees its locks. `force`
needs `force:`. `state.locks` maps each lock to its holder's session, or
null.

## Watchdog

The owner's SDK runs watchdogs; the relay only carries what they report. A
lock with a watchdog is **tripped** (safe) unless its holder is alive and
sending. It trips when:

- there is no holder, or the holder has sent nothing yet;
- heartbeats stop for `heartbeat_loss_ms`;
- after messages have started, they stop for `message_stale_ms`;
- the owner's relay connection drops.

It stays tripped until the next accepted message. Trips and clears are
reported on the holder's echoes and on the broadcast's `.events` track.
Teleop's dead-man switch is exactly this, on the `drive` lock.

## Room state and events

As in 2.0: the events stream starts with a `state`, then sequenced events,
with fresh states at most every 100 ms.

```text
RoomState {
  room_id,
  broadcasts: [{ name, owner_sid, build,
                 rtt_ms, clock_offset_us, clock_error_us, quic,
                 tracks:   [{ index, info: TrackInfo, online,
                              stats: { frames, groups, fps, kbps, dropped,
                                       capture_to_publisher_ms } }],
                 channels: [{ index, info: ChannelInfo }],
                 locks:    [{ index, info: LockInfo, holder }] }],
  participants: [{ session, sid, sub, perms, build, joined_us }],
}
```

Relay events: `participant_joined`, `participant_left`,
`broadcast_started`, `broadcast_ended {reason: ended | disconnected | replaced}`,
`track_published`, `track_updated`, `track_unpublished`, `track_online`,
`track_offline`, `lock_changed`. Unknown event and message types are
ignored, so minors can add them.

## Clocks and latency

Every frame carries the times it reached each stage:

| Stage | Clock | Set by |
| --- | --- | --- |
| `capture_us` | encoder (SEI) or owner | the encoder's LKTS SEI if present (`CAPTURE_FROM_SEI`), else the owner on receipt |
| `publisher_us` | owner | the owner once the access unit or message is complete |
| `relay_us` | relay | the relay on receipt |
| receive, decode, present | subscriber | the subscribing app, locally |

To put all of these on the subscriber's clock, each hop measures its offset
with control-stream `ping {id, t0_us}` / `pong {id, t0_us, t1_us}`. Each
side keeps the smallest-round-trip sample in a sliding window (the NTP clock
filter); half that round trip bounds the error.

- The relay measures **owner − relay** for every session that publishes, and
  puts it in the broadcast's state.
- Each session measures **relay − itself**.
- So any subscriber knows **owner − subscriber** as the sum, with the sum of
  both error bounds. Apps show the bound next to every cross-machine number.

SEI capture times are on the encoder's clock. 3.0 assumes the encoder runs
on the owner's machine or on a clock synced with it. A source may declare
`capture_clock_offset_us` in its config when it knows better (for example,
an encoder on another box synced by PTP).

Glass-to-glass = present (subscriber) − capture (converted). The SDK's
`Clock` type does the conversion. Apps never subtract raw timestamps taken
on different machines.

## Multiple broadcasts

The wire format allows any number of broadcasts per room and per session.
Phase 1 aims to support many per room, one per session. If that proves
expensive, the relay may cap a room at one broadcast in 3.0 and lift the
cap in a minor. Nothing on the wire changes either way: indices are already
per broadcast on the owner hop and room-wide on the subscriber hop.

## Close codes

| Code | Meaning |
| --- | --- |
| 1 `GROUP_SUPERSEDED` | a newer group replaced this one (stream reset) |
| 2 `GROUP_LATE` | the receiver fell too far behind (stream reset) |
| 3 `REPLACED` | another session took over this session's broadcast |
| 4 `PROTOCOL_ERROR` | |
| 5 `SHUTDOWN` | |
| 6 `UNSUPPORTED_VERSION` | different protocol major |
| 7 `FORBIDDEN` | the token does not allow this room |

## Changes from 2.0

| 2.0 | 3.0 |
| --- | --- |
| `/v1/publish/{room}` and `/v1/session/{room}` | one endpoint, `/v1/room/{room}` |
| publisher vs operator vs viewer | sessions; any of them may publish broadcasts |
| `announce {cameras}` | `advertise {broadcast, tracks, channels, locks}` |
| every client gets every camera | explicit `subscribe` / `unsubscribe`; owners see demand |
| group header `camera:u16`, version 1 | `track:u16` (plus `broadcast:u16` on the owner hop), version 2; per-track priority |
| `Role` in the token | `perms` with path patterns; `sid` in `welcome` |
| datagrams 0x10–0x13, teleop only, claim holder only | 0x20–0x23, per channel or lock, `sid` routing |
| `claim {force}`, `release`, `operator_changed` | `claim {lock, force}`, `release {lock}`, `lock_changed` |
| `TeleopStatus`, `deadman_*` events | `.status` and `.events` data tracks; watchdog edges |
| `state.publisher`, `state.operator` | `state.broadcasts[]` with tracks, channels, locks and holders |
