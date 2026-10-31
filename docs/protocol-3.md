# tq protocol 3.0

Status: implemented in `crates/tq-wire` (wire types), `crates/tq-relay`
and the `crates/tq` SDK; `web/src/protocol.ts` mirrors the wire types. Where
this document and the code disagree, the code is right and this is a bug.
3.0 replaces 2.0 (`tq-proto`) and does not interoperate with it. Items
marked *planned* are not implemented yet.

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
| **broadcast** | A named bundle a session publishes: tracks, channels and locks. A robot publishes `rover-7`; an operator's console might publish `alice` with a gamepad track. Names are unique in a room. |
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
  - **Datagrams**: channel messages, lock heartbeats, acks and echoes, and
    the frames of datagram tracks.

Names (broadcasts, tracks, channels, locks) are 1–64 of `[A-Za-z0-9_.-]`.

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

Presets (`tq-relay token --preset`, `/api/dev/token?preset=`): **operator**
= `sub:*/* write:*/* lock:*/* force:*/*`, **viewer** = `sub:*/*`, **robot**
= `pub:<name>` (never minted over HTTP). A string that does not parse allows
nothing but is kept, so a token from a newer minor still works for the rest.

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

- The relay answers `advertise_ok {broadcast, index, relay_us}` or
  `advertise_error {broadcast, reason}`. `index` is the broadcast's index
  on this session's owner hop (see [Indices](#indices)). The relay diffs each
  new advertisement against the previous one and emits `track_published`,
  `track_updated` and `track_unpublished` events.
- `unadvertise {broadcast}` removes a broadcast at once.
- Once a second, an owner may send `track_stats {broadcast, tracks: [{id,
  dropped}]}`: what it dropped before the relay. The relay measures the rest
  (frames, fps, kbps, capture latency) itself.
- A newer session advertising the same broadcast name replaces the older
  one, whose session closes with `REPLACED` if that was its only broadcast.
  This is how a robot that reconnects takes its place back.
- `watchdog` is informational. The owner enforces it, and apps can show it.
- Track `meta` and channel `schema` are optional free-form JSON for apps
  (units, ranges, field descriptions). The relay passes them through.
- Track ids that start with `.` are conventions:
  - `.status`: low-rate device state (mode, battery, faults), `latest`.
  - `.events`: anything the owner wants in an event log, `stream`. The SDK
    writes its watchdog edges here (`{"type": "watchdog_tripped", "lock",
    "reason"}`, `{"type": "watchdog_cleared", "lock"}`).

  They are ordinary data tracks, so the same permissions apply (a token can
  allow `sub:rover-7/.status` and nothing else).

### Indices

Group streams and datagrams name things by two `u16`s: a broadcast index
and a track, channel or lock index.

- The **track, channel or lock index** is its position in the broadcast's
  latest advertisement, on every hop. The room state lists it.
- The **broadcast index** depends on the hop. Owner ↔ relay: the owner's
  own index, from `advertise_ok` (its broadcasts in advertise order).
  Relay ↔ subscriber: the room's index, from the room state, which stays
  the same for as long as the broadcast exists.

The relay rewrites the broadcast index as it forwards. A re-advertisement
that reorders a broadcast's lists changes the other indices; receivers
follow the room state.

## Subscribing

A session gets only the tracks it asks for:

```text
subscribe   { tracks: ["rover-7/front", { "track": "rover-7/imu", "max_rate_hz": 60, "policy": "smooth" }] }   (adds)
unsubscribe { tracks: ["rover-7/front"] }
subscribe_result { track, ok, reason }
keyframe_request { track, reason }      (subscriber -> relay -> owner, at most 1 per track per 500 ms)
```

- Each track needs `sub:`. Subscribing again replaces the options.
  Subscriptions persist when a track goes offline and comes back.
- `policy` picks the relay's drop policy for this subscriber: `realtime`
  (default; drop to the next group as soon as a frame is 250 ms late) or
  `smooth` (1.5 s, and older groups may finish). The relay's flags set both.
- A subscription to a track that does not exist yet waits for it.
- The relay forwards a track from its owner only while someone subscribes,
  and tells the owner with `track_demand {track, subscribers}`. An owner may
  stop encoding idle tracks.
- Apps subscribe to what their widgets are bound to. A hidden widget
  unsubscribes, so no bandwidth goes to it.

## Group streams

```text
group stream := type:u8 (=0x01) version:u8 (=2) broadcast:u16 track:u16
                group:u64 codec_len:u8 codec:utf8[codec_len]
                frame*
frame        := len:u32 index:u32 flags:u8 capture_us:u64 publisher_us:u64
                relay_us:u64 frame_id:u32 payload
```

2.0's format with `camera:u16` replaced by `broadcast:u16 track:u16` and the
version bumped to 2. Flags: bit 0 `KEY`, bit 1 `CAPTURE_FROM_SEI`. A frame
holds at most 16 MiB. Group numbers increase per track; each node renumbers
them, so they only mean something on one hop.

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
- **Batching** (*planned* SDK option, `stream` only): the owner packs
  samples that arrive within `batch_ms` into one frame, each with its own
  time offset. This cuts per-frame overhead at kHz rates, at the cost of up
  to `batch_ms` of extra latency. Off by default.

Datagram track frames:

```text
0x24 track_frame  broadcast:u16 track:u16 seq:u32 flags:u8 capture_us:u64
                  publisher_us:u64 relay_us:u64 payload
```

The relay writes `relay_us` and the room's broadcast index once and sends
the same bytes to every subscriber. It keeps the newest frame for new
subscribers. The whole datagram
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
0x20 message    sid:u32 broadcast:u16 channel:u16 seq:u32 sent_us:u64 payload
0x21 ack        sid:u32 broadcast:u16 channel:u16 seq:u32 echo_us:u64 status:u8 detail:utf8
0x22 heartbeat  sid:u32 broadcast:u16 lock:u16    seq:u32 sent_us:u64
0x23 echo       sid:u32 broadcast:u16 lock:u16    seq:u32 echo_us:u64 flags:u8
```

Type `0x24` is a datagram track frame; see [High-rate data](#high-rate-data).

- Indices follow the [rules above](#indices).
- `sid` routes: a sender writes 0, and the relay writes the sender's `sid`
  on the way to the owner. The owner writes the target's `sid` on acks and
  echoes, and the relay forwards them to that session only. The fixed
  layout lets the relay rewrite `sid` and `broadcast` in place.
- **Relay checks:** a message needs `write:` on the channel, and, if the
  channel belongs to a lock, the sender must hold it. A heartbeat needs the
  sender to hold the lock. Anything else is dropped and counted; it never
  reaches the owner.
- **Sequencing:** latest wins per channel and sender. A `seq` at or below
  the last one applied is acked `superseded` and not applied (the SDK does
  this before the app sees the message). The sequence restarts when a
  lock's holder changes.
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
relay -> session: claim_result { lock, ok, holder, reason, claim }
relay -> owner:   lock_holder  { lock, sid, session, claim }   (null = free)
event:            lock_changed { lock, from, to, reason, from_claim, to_claim }
                  reason: claimed | forced | released | left
```

The relay owns locks. It numbers claims (`claim`, a `u64` that only
grows), so a new claim differs from an old one even when the same session
makes both, and from the relay restating holders to an owner that
reconnects. `claim_result` gives the claimant its number; `lock_changed`
carries the claim that ended (`from_claim`) and the one that began
(`to_claim`). Claim results travel on the control stream and lock events
on the events stream, so they can arrive in either order: a holder
follows its own claim by number, not by session. A holder's session ending frees its locks. Claiming
needs `lock:` (or `force:`); taking a held lock needs `force:` and
`force: true`. Holders survive an owner's outage: when the owner comes
back, it is told the current holders again. The room state lists each
lock's holder.

## Watchdog

The owner's SDK runs watchdogs; the relay only carries what they report. A
lock with a watchdog is **tripped** (safe) unless its holder is alive and
sending. It trips when:

- there is no holder, or the holder has sent nothing yet;
- heartbeats stop for `heartbeat_loss_ms`;
- after messages have started, they stop for `message_stale_ms`;
- the owner's relay connection drops.

It stays tripped until the next accepted message (the owner's app acks it
`ok`). Trips and clears are reported on the holder's echoes and on the
broadcast's `.events` track. Teleop's dead-man switch is exactly this, on
the `drive` lock (`crates/tq-teleop`).

Teleop adds an e-stop on top (an application convention, not part of the
wire): a message on the open `estop` channel stops the robot whoever
drives it, and latches. Drive commands are rejected until the drive lock
is claimed again, at least 500 ms after the e-stop (a claim made around
the same time may have raced it). Consoles send an e-stop again every
50 ms until it is acked, since a datagram can be lost; the robot treats
repeats within 500 ms as one press.

## Room state and events

As in 2.0: the events stream (type `0x02`, version 2) starts with a
`state`, then sequenced events, with fresh states at most every 100 ms.

```text
RoomState {
  room_id,
  broadcasts: [{ index, name, owner, owner_sid, build,
                 rtt_ms, clock_offset_us, clock_error_us, quic,
                 tracks:   [{ index, info: TrackInfo, online,
                              stats: { frames, groups, fps, kbps, dropped,
                                       capture_to_publisher_ms, codec_string,
                                       subscribers } }],
                 channels: [{ index, info: ChannelInfo }],
                 locks:    [{ index, info: LockInfo, holder }] }],
  participants: [{ session, sid, sub, perms, build, joined_us }],
}
```

Relay events: `participant_joined`, `participant_left`,
`broadcast_started {broadcast, owner, build}`,
`broadcast_stopped {broadcast, reason: disconnected | replaced | unadvertised}`
(the broadcast stays, offline, for the grace period),
`broadcast_removed {broadcast}`, `track_published {track, info}`,
`track_updated {track, info}`, `track_unpublished {track}`,
`track_online {track}`, `track_offline {track, reason}`,
`lock_changed {lock, from, to, reason}`. A publishing session sends
`track_online` and `track_offline` for its own tracks; the relay sends the
rest. Unknown event and message types are ignored, so minors can add them.

Every second the relay also sends each session `link_stats {quic}`, the
QUIC stats of its own connection.

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
on the owner's machine or on a clock synced with it. *Planned:* a source
may declare `capture_clock_offset_us` in its config when it knows better
(for example, an encoder on another box synced by PTP).

Glass-to-glass = present (subscriber) − capture (converted). In the SDK,
`Session::owner_offset(broadcast)` gives owner − me as a `tq_wire::Offset`
whose `to_local()` does the conversion. Apps never subtract raw timestamps
taken on different machines.

## Multiple broadcasts

Any number of broadcasts per room and per session, from different
sessions. The relay supports this as implemented; the consoles (web,
tq-app) currently drive only the first broadcast with a `drive` lock.

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
| group header `camera:u16`, version 1 | `broadcast:u16 track:u16`, version 2; per-track priority |
| `Role` in the token | `perms` with path patterns; `sid` in `welcome` |
| datagrams 0x10–0x13, teleop only, claim holder only | 0x20–0x23, per channel or lock, `sid` routing; 0x24 datagram track frames |
| `claim {force}`, `release`, `operator_changed` | `claim {lock, force}`, `release {lock}`, `lock_changed` |
| `publisher_status`, `TeleopStatus`, `deadman_*` events | relay-measured track stats, `track_stats` from owners; `.status` and `.events` data tracks |
| `state.publisher`, `state.operator` | `state.broadcasts[]` with tracks, channels, locks and holders |
