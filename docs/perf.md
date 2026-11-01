# Performance bench

`dev/bench/bench.ts` runs the whole stack on one machine through fixed
scenarios, writes one JSON result, and compares two results. It exists to
catch latency regressions while the code changes quickly, so it measures
what an operator feels, hop by hop, and checks that the measurements are
right.

```bash
bun dev/bench/bench.ts list                  # scenarios; * = in --quick
bun dev/bench/bench.ts run --quick           # about 4 min
bun dev/bench/bench.ts run                   # everything, about 8 min
bun dev/bench/bench.ts run --only video-direct,join
bun dev/bench/bench.ts run --app             # also tq-app's decode and paint (opens a window)
bun dev/bench/bench.ts run --ref main        # build a ref in a worktree (/tmp/tq-bench-*) and bench it
TQ_SHARED_SECRET=$(terraform -chdir=infra/gcp output -raw shared_secret) \
  bun dev/bench/bench.ts run --relay https://tq-relay-1.haffi.dev   # a deployed relay
bun dev/bench/bench.ts compare BASE.json NEW.json [--fail]
bun dev/bench/bench.ts report RESULT.json
```

`run` builds the working tree in release mode first (skip it with
`--no-build`, or point `--bin-dir` at other binaries). Results go to
`target/bench/<time>-<label>.json`. Every process's log is kept in the
`-logs` directory next to the result. The bench uses its own ports (QUIC
14443, HTTP 18080, impairment 14444, sources 15004/15010/15011, data
16000–16002), so a `dev/run-local.sh` stack can stay up. It shares the
CPU and the hardware encoder, though, so stop it before taking numbers
you will compare.

Agents: the `perf` skill (`.claude/skills/perf/SKILL.md`) has the
procedure.

## What it runs

Sources are `gstreamer-source` from streaming-tools with fixed settings,
VideoToolbox (or the platform's hardware encoder), with SEI capture
timestamps and an IDR every second:
- **pair**: H.264 and H.265 test patterns, 1280×720 at 30 fps, 2 Mbps
  target (the ball pattern encodes to about 0.4–0.7 Mbps).
- **prodlike**: one H.265 camera at 10 fps, 2 Mbps, length-prefixed, on
  the snow pattern, so it really is 2 Mbps (frames of about 25 KB).

| Scenario | What it measures |
| --- | --- |
| `video-direct` | pair, no impairment. An operator probe subscribes to everything and drives at 20 Hz. Per-hop video latency, teleop ack and heartbeat round trips. |
| `video-starlink` | the same, with the robot's uplink through `tq-impair --profile starlink` |
| `prodlike-direct`, `-starlink`, `-starlink-bad` | the prodlike camera, direct and through both Starlink profiles |
| `stop`, `stop-starlink` | the remote stop: 10 fresh operator sessions each send one `stop` and time its ack |
| `data` | data tracks at once: `latest` 50 Hz, `stream` 100 Hz, `datagram` 200 Hz, about 200 B of JSON each. Rate delivered, edge→probe latency per hop. |
| `join`, `join-starlink` | subscribe → first frame, and → first *live* frame (capture→probe ≤ 100 ms, 200 ms on Starlink). 10 **cold** joins (nobody subscribed, so tq-edge is not sending) and 10 **warm** ones (another viewer is already in). |
| `fanout-10` | 10 viewer probes at once; each one's p50 and p95, the lowest fps |
| `--app` | tq-app (viewer, `--log-stats`) beside the probe in the video scenarios: decode, capture→decoded, decoded→painted, capture→render |

Every scenario also records the CPU the relay and tq-edge used.

The latency numbers in each video and data scenario leave out each
track's first 3 s (`tq-probe --warmup`). Those seconds are the join,
which has its own scenario. Spikes in the steady state are counted
(`>50 / >100 ms` frames) rather than hidden in a percentile.

### The hops

| Number | From → to | Covers |
| --- | --- | --- |
| `capture→pub` | the encoder's SEI capture time → tq-edge has the whole access unit | the encoder, TCP, AU assembly. Mostly the encoder: about 15 ms for VideoToolbox at 720p30. |
| `pub→relay` | tq-edge → the relay received it | the uplink, and tq-edge's send queue |
| `relay→probe` | the relay → the probe read it | the relay's fan-out and the downlink |
| `capture→probe` | capture → probe | all of it. Glass to glass adds the sensor (before the capture stamp), decode and display. See README "Measuring". |

### Are the numbers right? (`clock_check`)

Across machines, every hop but `capture→pub` relies on the in-band clock
sync (NTP-style, per hop, with an error bound). On one host every clock
is the same clock, so the raw timestamp differences are exact. With
`--same-host`, tq-probe compares each offset-corrected capture→probe
with the raw one and reports:
- `abs_error_ms`: how far off the correction was;
- `claimed_bound_ms`: the error bound the sync claimed;
- `within_bound`: the share of frames where the error was inside the
  bound. This must be about 1.0. If it is not, the latency numbers
  themselves are wrong, and that comes before any regression.

This checks the arithmetic and the offset estimation on a link with no
asymmetry. It cannot see what an asymmetric real link does to the offset;
the claimed bound (half the best round trip) covers that by construction.

## Comparing

`compare` lines up every latency metric (p50 and p95) of two results.
It marks a metric REGRESSED when it got worse by more than both:
- p50: 15% and 2 ms;
- p95: 25% and 5 ms.

`--fail` exits 1 if anything regressed. Only compare results from the
same machine.

### Noise

The sources and the impairment proxy are not deterministic. On an M1
MacBook Air:
- direct capture→probe p50 moves by about ±1 ms between runs, p95 by
  about ±1 ms, and max by tens of ms (one slow encode is enough);
- Starlink runs depend on where the 15 s handovers land: p95 moves by
  5–50 ms between runs (most on starlink-bad);
- join times depend on where in the 1 s GOP the join lands, so they
  spread over 0–1000 ms; compare medians over many joins.

So bench the base and the change interleaved (base, change, base, change)
for at least two rounds, and trust only differences that repeat.

### Against a deployed relay (`--relay`)

`--relay https://host` uses a relay that is already running, with a real
certificate, instead of starting one. tq-edge, the sources and the probes
stay on this machine. The relay's token secret comes from
`TQ_SHARED_SECRET` and is kept out of the configs written to the logs.
The internet path is the link, so the emulated-link scenarios are
skipped, and the relay's CPU is not measured (`null`). The probes use
room `robot-01` on that relay, so do not run it while someone is
driving a real robot there.

`clock_check` still holds, and means more here. Owner and probe are on
one clock, but the correction goes through the real relay over a real,
possibly asymmetric path. The per-hop split (pub→relay, relay→probe)
depends on the relay's clock, which the bench cannot check; their sum
can be checked.

Only compare remote results with remote results from the same network.
`compare` warns when the relays differ.

### Protocol 2.0 baselines

The bench can run protocol 2.0 builds (`tq-publisher`, before tq-edge):
`--ref c4a3782`. `edgeConfig()` writes the old config and `probe()` maps
the old probe's output. 2.0 probes have no `--warmup`, `--same-host`,
`--send` or join numbers, so `stop`, `data` and `join` are skipped, and
their video numbers include the join. Delete the legacy paths once no
baseline worth comparing against predates 3.0.

## Results

### 2026-11-01: protocol 3.0 (5ab0659) against 2.0 (c4a3782)

On an M1 MacBook Air (macOS 15.6), interleaved: 2 rounds of the full
suite per build, plus 3 more rounds of the direct video scenarios, plus 2
rounds with tq-app. Means of the rounds. Emulated links, not real Starlink.

**No regression.** No metric moved past the margins in either round, and
every difference is inside the run-to-run noise of the same build.

capture→probe and pub→relay, p50 / p95 ms:

| Scenario | Track | 2.0 capture→probe | 3.0 capture→probe | 3.0 capture→pub p50 | 2.0 pub→relay | 3.0 pub→relay |
| --- | --- | --- | --- | --- | --- | --- |
| video-direct | front | 16.8 / 19.6 | 17.1 / 19.6 | 16.5 | 0.2 / 0.4 | 0.2 / 0.4 |
| video-direct | rear | 17.4 / 20.2 | 17.8 / 20.1 | 17.2 | 0.2 / 0.4 | 0.2 / 0.4 |
| video-starlink | front | 45.2 / 55.4 | 45.0 / 56.3 | 14.2 | 30.9 / 38.8 | 31.0 / 40.8 |
| video-starlink | rear | 43.4 / 54.2 | 44.5 / 55.0 | 15.2 | 29.2 / 37.5 | 29.5 / 38.8 |
| prodlike-direct | front | 25.2 / 27.1 | 24.9 / 27.0 | 23.4 | 0.5 / 1.4 | 0.5 / 1.5 |
| prodlike-starlink | front | 67.9 / 131.8 | 66.8 / 127.2 | 23.2 | 44.5 / 105.4 | 43.2 / 101.9 |
| prodlike-starlink-bad | front | 107.8 / 284.6 | 112.2 / 260.3 | 23.4 | 84.1 / 260.3 | 88.1 / 238.6 |

Steady state, 3.0 had no frame over 50 ms on direct links. relay→probe
is 0.2–0.9 ms everywhere. The encoder dominates direct latency: about
15–17 ms of capture→pub at 30 fps, and 23 ms for 25 KB frames at 10 fps.

Teleop ack round trip, p50 / p95 ms: direct 0.6 / 1.0 → 0.6 / 0.8;
starlink 57 / 68 → 58 / 68; prodlike-starlink 60 / 99 → 61 / 91;
starlink-bad 96 / 214 → 100 / 217. Every command was acked `Ok`.

Glass to glass (decode, paint, display):

| Console | Number | 2.0 | 3.0 |
| --- | --- | --- | --- |
| tq-app 0.4 → 0.5, video-direct | capture→decoded / decoded→painted / capture→render | 15.7 / 8.8 / 24.5 | 16.6 / 8.9 / 25.6 |
| tq-app, prodlike-direct | the same | 28.2 / 11.9 / 40.0 | 27.2 / 11.9 / 39.2 |
| web 0.2 → 0.3 (Chrome 152), pair, direct | capture→drawn / glass→glass≈ | 16.6 / 40.6 | 16.2 / 40.0 |

tq-app's decoded→painted moves ±4 ms between rounds (the wait for the
display link). Its video-direct capture→decoded was 1–2 ms higher in both
rounds, all of it in capture→pub: with the app open, its VideoToolbox
decode shares the hardware with the test encoder. Without the app, the
same scenario over 5 rounds shows no difference (rear capture→probe p50
17.0 vs 17.1 ms). The web numbers are one 20 s sample per build in an
embedded Chromium; its `draw→display≈` estimate is unreliable there.

**Clock sync.** Every frame of every scenario was within the claimed
bound. Direct: |error| ≤ 0.2 ms against a claimed 0.2–0.4 ms. Starlink:
|error| ≤ 6 ms against a claimed 20–30 ms; the bound is conservative by
about 4×, as it should be.

**CPU.** The relay and tq-edge each use 1–2.5% of one core per scenario
(3.0 about 0.1–0.2 points more than 2.0); 10 subscribers cost the relay
5–6%. Data at 350 msg/s: relay 6%, edge 4%.

New in 3.0, so there is no 2.0 number:

| Scenario | Result |
| --- | --- |
| `stop` | remote stop ack: p50 0.8–0.9 ms, p95 1.4–4.1 ms, 10/10 `Ok` |
| `stop-starlink` | p50 61–63 ms, p95 69–70 ms, 10/10 `Ok` |
| `data` | all rates delivered (50 / 100 / 200 Hz); edge→probe p50 0.5–0.7 ms, p95 under 1 ms |
| `fanout-10` | every probe 29.6+ fps; p50 15–16.5 ms, p95 18.5–19.5 ms, the same as one probe |
| `join` cold | first live frame p50 130–190 ms, p95 450–560 ms |
| `join` warm | p50 370–480 ms, p95 630–700 ms |
| `join-starlink` | cold p50 200–280 ms, warm p50 460–510 ms |

### 2026-11-01: protocol 3.0 (5ab0659) through the GCP relay

Relay 0.4.0 on `tq-relay-1` (e2-standard-2, London). Bench on the M1
MacBook Air at home, about 8 ms round trip to the VM. Two full runs, with
tq-app. Both runs are shown where they differ.

| Scenario | Result |
| --- | --- |
| `video-direct` (2 × 720p30) | capture→probe p50 / p95 22.7–24.7 / 27–28 ms: capture→pub 13.5–15.4, pub→relay 3.8–4.2 / 5, relay→probe 4.8–6.1 / 7–8. At most 1 frame over 50 ms in a run. |
| `prodlike-direct` (2 Mbps H.265, 10 fps) | 38.4 / 50–54 ms: capture→pub 23, pub→relay 5 / 7.4, relay→probe 10–11 / 21–22. 16–26 frames per run over 50 ms, none over 100. |
| teleop | ack round trip 17 / 19–28 ms, 680/680 `Ok` |
| `stop` | remote stop ack 18–19 / 20 ms, 10/10 `Ok` |
| `data` | all rates delivered; edge→probe 7–10 / 10–12 ms |
| `join` | cold p50 200–230 ms; warm p50 310–430 ms |
| `fanout-10` | every probe at 30 fps; p50 26–27 ms, p95 26–28 ms across probes |
| tq-app, video-direct | decode 2 ms, capture→decoded 24–26 ms, capture→render 30–38 ms |
| tq-app, prodlike | capture→decoded 40 ms, capture→render 47–49 ms |
| clock | \|error\| ≤ 1.4 ms against a claimed 6.5–7.9 ms; 100% within the bound |

Compared with a local relay, the internet adds about 8–9 ms to
capture→probe at 720p30, which is the round trip. 2 Mbps frames (about 25 KB)
come down slower than they go up: relay→probe is twice pub→relay, and
its p95 is 21 ms. That is the downlink from London at home. It is
also where the frames over 50 ms come from.

### Joining is GOP-bound

A new subscriber needs a keyframe. The relay replays its cached group
(from the keyframe), but it drops the frames that arrived at the relay
more than `--realtime-max-lag-ms` (250 ms) ago. Unless the group is that
young, the viewer waits for the next IDR. With a 1 s GOP, that is about
375 ms on average, plus the time to get there. Protocol 2.0's relay did the same, so
warm joins have not changed.

A **cold** join (nobody was subscribed) is a 3.0 path. tq-edge then sends
its own cache with its own 400 ms limit, and the relay stamps those frames
on arrival, so they pass its check. That is why cold joins are faster
than warm ones. The catch-up frames are older than they look to the
relay: a probe without `--warmup` sees them as a pub→relay spike of up to
about 60 ms in its first second.

Joins could be faster by replaying the whole cached group to a new
subscriber regardless of age, letting the decoder catch up and showing
only the newest picture. Or the edge could get an encoder hook for
keyframe requests (README "Not done yet").
