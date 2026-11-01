// Performance regression bench: runs the stack on this machine through
// fixed scenarios and writes one JSON result; `compare` diffs two results.
//
//   bun dev/bench/bench.ts run [--quick] [--only video-direct,data] [--app]
//                              [--bin-dir DIR] [--label NAME] [--out FILE]
//   bun dev/bench/bench.ts run --ref c4a3782     # build a git ref in a worktree, bench it
//   TQ_SHARED_SECRET=... bun dev/bench/bench.ts run --relay https://tq-relay-1.haffi.dev
//                                                # against a deployed relay; see docs/perf.md
//   bun dev/bench/bench.ts run --save-baseline   # full run, then keep it in dev/bench/baselines/
//   bun dev/bench/bench.ts compare BASE.json NEW.json [--fail]
//   bun dev/bench/bench.ts compare NEW.json      # against the committed baseline for its machine and relay
//   bun dev/bench/bench.ts list
//
// Everything runs on one host, so every clock is the same clock: the bench
// also checks the in-band clock sync against the truth (`clock_check`).
// Results are only comparable between runs on the same machine, with the
// same sources; see docs/perf.md.
//
// Needs: a release build (`cargo build --release`, done by `run` unless
// --bin-dir or --no-build), bun, and streaming-tools next to this repo (or
// STREAMING_TOOLS=...) for gstreamer-source.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const ROOT = resolve(import.meta.dir, '../..');
const STREAMING_TOOLS = process.env.STREAMING_TOOLS ?? join(ROOT, '../streaming-tools');
let SECRET = 'bench-secret';
// Off the ports dev/run-local.sh uses, so a running dev stack is left alone.
const QUIC = 14443;
const HTTP = 18080;
const IMPAIR = 14444;
let RELAY = `https://127.0.0.1:${QUIC}`;
let CERT_URL = `http://127.0.0.1:${HTTP}/api/cert`;
let RELAY_HTTP = `http://127.0.0.1:${HTTP}`;
/** `--relay`: a deployed relay (real certificate) instead of a local one. */
let REMOTE: string | null = null;
const SRC = { h264: 15010, h265: 15011, prodlike: 15004 };
const DATA = { latest: 16000, stream: 16001, datagram: 16002 };

// ---------------------------------------------------------------- scenarios

type Source = { port: number; args: string[] };
type Scenario = {
  name: string;
  what: string;
  sources: 'pair' | 'prodlike' | 'none';
  data?: boolean;
  impair?: 'starlink' | 'starlink-bad';
  /** Seconds the probe measures; impaired links need >15 s for a handover. */
  secs: number;
  quickSecs: number;
  kind: 'video' | 'stop' | 'fanout' | 'data' | 'join';
  probes?: number;
  quick?: boolean;
  /** Not supported by protocol 2.0 builds. */
  v3Only?: boolean;
  /** tq-app runs beside the probe; only with --app. Kept out of the other
   * scenarios: its decode shares the hardware with the test encoder and
   * moves capture→pub by a few ms. */
  app?: boolean;
};

const SCENARIOS: Scenario[] = [
  { name: 'video-direct', what: 'H.264 + H.265 720p30, no impairment, operator driving at 20 Hz', sources: 'pair', secs: 30, quickSecs: 15, kind: 'video', quick: true },
  { name: 'video-starlink', what: 'as video-direct, robot uplink through the starlink profile', sources: 'pair', impair: 'starlink', secs: 45, quickSecs: 20, kind: 'video', quick: true },
  { name: 'prodlike-direct', what: 'one H.265 720p10 2 Mbps camera (snow), length-prefixed', sources: 'prodlike', secs: 30, quickSecs: 15, kind: 'video' },
  { name: 'prodlike-starlink', what: 'prodlike through starlink', sources: 'prodlike', impair: 'starlink', secs: 45, quickSecs: 20, kind: 'video', quick: true },
  { name: 'prodlike-starlink-bad', what: 'prodlike through starlink-bad', sources: 'prodlike', impair: 'starlink-bad', secs: 45, quickSecs: 20, kind: 'video' },
  { name: 'stop', what: 'remote stop: 10 fresh operator sessions, each sends a stop (resent every 50 ms until acked) and times the ack', sources: 'pair', secs: 40, quickSecs: 40, kind: 'stop', quick: true, v3Only: true },
  { name: 'stop-starlink', what: 'remote stop through starlink', sources: 'pair', impair: 'starlink', secs: 40, quickSecs: 40, kind: 'stop', v3Only: true },
  { name: 'data', what: 'data tracks: latest 50 Hz, stream 100 Hz, datagram 200 Hz, ~200 B JSON', sources: 'none', data: true, secs: 20, quickSecs: 10, kind: 'data', quick: true, v3Only: true },
  { name: 'join', what: 'join to live video: 10 cold joins (edge idle, nobody subscribed), then 10 warm (another viewer already in)', sources: 'pair', secs: 3, quickSecs: 3, kind: 'join', quick: true, v3Only: true },
  { name: 'join-starlink', what: 'join through starlink', sources: 'pair', impair: 'starlink', secs: 3, quickSecs: 3, kind: 'join', v3Only: true },
  { name: 'app-direct', what: 'video-direct with tq-app (viewer) open: decode, decoded→painted, capture→render', sources: 'pair', secs: 30, quickSecs: 15, kind: 'video', quick: true, app: true },
  { name: 'app-prodlike', what: 'prodlike-direct with tq-app open', sources: 'prodlike', secs: 30, quickSecs: 15, kind: 'video', app: true },
  { name: 'fanout-10', what: '10 viewer probes on the video-direct stream at once', sources: 'pair', secs: 20, quickSecs: 10, kind: 'fanout', probes: 10 },
];

// ---------------------------------------------------------------- processes

const procs: Bun.Subprocess[] = [];
/** The data feeders' listeners and timers, if a scenario started them. */
let stopFeeders = () => {};
function spawn(cmd: string[], opts: { cwd?: string; log?: string; env?: Record<string, string> } = {}) {
  const p = Bun.spawn(cmd, {
    cwd: opts.cwd ?? ROOT,
    env: { ...process.env, TQ_SHARED_SECRET: SECRET, RUST_LOG: process.env.RUST_LOG ?? 'warn', ...opts.env },
    stdout: opts.log ? Bun.file(opts.log) : 'ignore',
    stderr: opts.log ? Bun.file(opts.log + '.err') : 'ignore',
  });
  procs.push(p);
  return p;
}
async function stopAll() {
  stopFeeders();
  stopFeeders = () => {};
  for (const p of procs.splice(0)) p.kill();
  await Bun.sleep(500);
}
process.on('SIGINT', async () => {
  await stopAll();
  process.exit(130);
});

async function sh(cmd: string[], cwd = ROOT): Promise<string> {
  const p = Bun.spawn(cmd, { cwd, stdout: 'pipe', stderr: 'inherit' });
  const out = await new Response(p.stdout).text();
  if ((await p.exited) !== 0) throw new Error(`${cmd.join(' ')} failed`);
  return out;
}

async function waitHttp(url: string, ms = 10000) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await Bun.sleep(200);
  }
  throw new Error(`no answer from ${url}`);
}

async function portFree(port: number) {
  try {
    const s = await Bun.connect({ hostname: '127.0.0.1', port, socket: { data() {} } });
    s.end();
    return false;
  } catch {
    return true;
  }
}

/** CPU seconds a process has used so far (macOS and Linux `ps`). */
async function cpuSecs(pid: number): Promise<number | null> {
  try {
    const t = (await sh(['ps', '-o', 'time=', '-p', String(pid)])).trim();
    // [[dd-]hh:]mm:ss.cc
    const [days, rest] = t.includes('-') ? t.split('-') : ['0', t];
    return rest.split(':').map(Number).reduce((a, x) => a * 60 + x, 0) + Number(days) * 86400;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------- builds

type Build = { binDir: string; legacy: boolean; label: string; commit: string };

async function prepareBuild(args: Map<string, string | true>): Promise<Build> {
  const ref = args.get('ref');
  let binDir = args.get('bin-dir') as string | undefined;
  let commit = (await sh(['git', 'rev-parse', '--short', 'HEAD'])).trim();
  const dirty = (await sh(['git', 'status', '--porcelain'])).trim() !== '';
  if (typeof ref === 'string') {
    const wt = `/tmp/tq-bench-${ref.replace(/[^\w.-]/g, '_')}`;
    commit = (await sh(['git', 'rev-parse', '--short', ref])).trim();
    if (!existsSync(wt)) await sh(['git', 'worktree', 'add', '--detach', wt, ref]);
    // A moving ref (main) has moved since the worktree was made.
    else await sh(['git', 'checkout', '-q', '--detach', commit], wt);
    binDir = join(wt, 'target/release');
    console.error(`building ${ref} (${commit}) in ${wt}`);
    await sh(['cargo', 'build', '--release', '-q', '--workspace', '--exclude', 'tq-app'], wt).catch(() =>
      sh(['cargo', 'build', '--release', '-q'], wt),
    );
  } else if (!binDir) {
    binDir = join(ROOT, 'target/release');
    if (!args.has('no-build')) {
      console.error('cargo build --release');
      await sh(['cargo', 'build', '--release', '-q']);
      if (args.has('app')) await sh(['cargo', 'build', '--release', '-q', '-p', 'tq-app']);
    }
  }
  const legacy = !existsSync(join(binDir, 'tq-edge')) && existsSync(join(binDir, 'tq-publisher'));
  const label = (args.get('label') as string) ?? `${commit}${dirty && !ref ? '-dirty' : ''}`;
  return { binDir, legacy, label, commit: commit + (dirty && !ref ? '-dirty' : '') };
}

// ---------------------------------------------------------------- configs

function edgeConfig(s: Scenario, relayUrl: string, legacy: boolean): object {
  const video =
    s.sources === 'pair'
      ? [
          { id: 'front', label: 'Front (H.264)', port: SRC.h264, fps: 30 },
          { id: 'rear', label: 'Rear (H.265)', port: SRC.h265, fps: 30 },
        ]
      : s.sources === 'prodlike'
        ? [{ id: 'front', label: 'Front (H.265 10 fps 2 Mbps)', port: SRC.prodlike, fps: 10 }]
        : [];
  if (legacy) {
    // Protocol 2.0's tq-publisher (before tq-edge). Delete once no
    // baseline worth comparing against is older than 3.0.
    return {
      room_id: 'robot-01',
      relay_url: relayUrl,
      relay_trust: REMOTE ? { mode: 'system_roots' } : { mode: 'hash_url', url: CERT_URL },
      shared_secret: SECRET,
      max_lag_ms: 400,
      quic: { congestion: 'bbr', send_window: 1048576 },
      teleop: { heartbeat_loss_ms: 500, command_stale_ms: 500 },
      cameras: video.map((v) => ({
        id: v.id, label: v.label, source: `127.0.0.1:${v.port}`, codec: 'auto', framing: 'auto', width: 1280, height: 720, fps: v.fps,
      })),
    };
  }
  const data = s.data
    ? (['latest', 'stream', 'datagram'] as const).map((d) => ({
        id: `data-${d}`,
        label: `Data (${d})`,
        kind: 'data',
        data: { encoding: 'json', delivery: d, rate_hz: DATA_HZ[d] },
        source: { type: 'tcp', addr: `127.0.0.1:${DATA[d]}` },
      }))
    : [];
  return {
    room: 'robot-01',
    relay: {
      url: relayUrl,
      trust: REMOTE ? { mode: 'system_roots' } : { mode: 'hash_url', url: CERT_URL },
      // A real relay's secret stays out of the config (it is kept with
      // the logs); tq-edge takes TQ_SHARED_SECRET from the environment.
      shared_secret: REMOTE ? '' : SECRET,
      max_lag_ms: 400,
      quic: { congestion: 'bbr', send_window: 1048576 },
    },
    tracks: [
      ...video.map((v) => ({
        id: v.id, label: v.label, kind: 'video', video: { width: 1280, height: 720, fps: v.fps },
        source: { type: 'tcp', addr: `127.0.0.1:${v.port}` },
      })),
      ...data,
    ],
    teleop: { heartbeat_loss_ms: 500, command_stale_ms: 500, sink: { type: 'log' } },
  };
}

const WARMUP_SECS = 3;
/** Joins per mode; join time depends on where in the GOP it lands, so it needs samples. */
const JOINS = 10;
/** capture→probe at or under this counts as live (join_live_ms). */
const liveMs = (s: Scenario) => (s.impair ? 200 : 100);

const DATA_HZ = { latest: 50, stream: 100, datagram: 200 } as const;

/** JSON-lines feeders for the data tracks; tq-edge connects to them. */
function startDataFeeders() {
  const servers: { stop(): void }[] = [];
  const pad = 'x'.repeat(140);
  for (const [d, port] of Object.entries(DATA) as [keyof typeof DATA, number][]) {
    const clients = new Set<{ write(s: string): number }>();
    let seq = 0;
    servers.push(
      Bun.listen({
        hostname: '127.0.0.1',
        port,
        socket: { open: (s) => void clients.add(s), close: (s) => void clients.delete(s), error: (s) => void clients.delete(s), data() {} },
      }),
    );
    // setInterval drifts late (200 Hz comes out near 160), so send what
    // is due by the clock on a fast tick.
    const start = performance.now();
    const t = setInterval(() => {
      const due = Math.floor(((performance.now() - start) / 1000) * DATA_HZ[d]);
      while (seq < due) {
        const line = JSON.stringify({ seq: seq++, t_us: Date.now() * 1000, pad }) + '\n';
        for (const c of clients) c.write(line);
      }
    }, 1);
    servers.push({ stop: () => clearInterval(t) });
  }
  return () => servers.forEach((s) => s.stop());
}

function startSources(s: Scenario, logDir: string) {
  const gst = (args: string[], name: string) =>
    spawn(['uv', 'run', 'gstreamer-source', '--timestamps', ...args], {
      cwd: join(STREAMING_TOOLS, 'tools/gstreamer_source'),
      log: join(logDir, `src-${name}.log`),
    });
  // Fixed encoder settings: results only compare when the video does.
  if (s.sources === 'pair') {
    gst(['--port', String(SRC.h264), '--bitrate', '2000'], 'h264');
    gst(['--codec', 'h265', '--stream-format', 'avc', '--port', String(SRC.h265), '--bitrate', '2000'], 'h265');
  } else if (s.sources === 'prodlike') {
    gst(['--codec', 'h265', '--stream-format', 'avc', '--fps', '10', '--bitrate', '2000', '--pattern', 'snow', '--port', String(SRC.prodlike)], 'prodlike');
  }
}

// ---------------------------------------------------------------- probe

type Summary = { n: number; mean: number; p50: number; p95: number; max: number } | null;

async function probe(build: Build, extra: string[], secs: number, log: string): Promise<any> {
  const args = ['--relay', RELAY, '--cert-url', CERT_URL, '--duration', String(secs), '--json', ...extra];
  if (build.legacy) {
    // Protocol 2.0's tq-probe: --role, no --same-host.
    const i = args.indexOf('--preset');
    if (i >= 0) args[i] = '--role';
  } else {
    // Steady state only; the join is measured on its own.
    args.push('--same-host', '--warmup', String(WARMUP_SECS));
  }
  const p = Bun.spawn([join(build.binDir, 'tq-probe'), ...args], {
    env: { ...process.env, TQ_SHARED_SECRET: SECRET, RUST_LOG: 'info,quinn=warn' },
    stdout: 'pipe',
    stderr: Bun.file(log),
  });
  const out = await new Response(p.stdout).text();
  await p.exited;
  const line = out.trim().split('\n').pop();
  if (!line) throw new Error(`tq-probe printed nothing; see ${log}`);
  return normalize(JSON.parse(line));
}

/** Protocol 2.0 probe output, in the 3.0 shape. */
function normalize(r: any): any {
  if (!r.cameras) return r;
  const tracks: Record<string, any> = {};
  for (const [cam, v] of Object.entries<any>(r.cameras)) tracks[`robot-01/${cam}`] = { kind: 'video', ...v };
  return { ...r, tracks, ack_rtt_ms: r.teleop_rtt_ms };
}

function pick(s: Summary) {
  return s ? { p50: s.p50, p95: s.p95, max: s.max, n: s.n } : null;
}

// ---------------------------------------------------------------- run

async function runScenario(s: Scenario, build: Build, secs: number, logDir: string) {
  for (const port of [...(REMOTE ? [] : [QUIC, HTTP, IMPAIR]), ...Object.values(SRC), ...Object.values(DATA)]) {
    if (!(await portFree(port))) throw new Error(`port ${port} is busy; stop whatever uses it`);
  }
  const relay = REMOTE
    ? null
    : spawn([join(build.binDir, 'tq-relay'), '--dev', '--quic', `127.0.0.1:${QUIC}`, '--http', `127.0.0.1:${HTTP}`], {
        log: join(logDir, `${s.name}-relay.log`),
      });
  await waitHttp(CERT_URL);
  let relayUrl = RELAY;
  if (s.impair) {
    spawn([join(build.binDir, 'tq-impair'), '--profile', s.impair, '--listen', `127.0.0.1:${IMPAIR}`, '--upstream', `127.0.0.1:${QUIC}`], {
      log: join(logDir, `${s.name}-impair.log`),
    });
    relayUrl = `https://127.0.0.1:${IMPAIR}`;
  }
  startSources(s, logDir);
  if (s.data) stopFeeders = startDataFeeders();
  await Bun.sleep(s.sources === 'none' ? 500 : 3000);
  const cfgPath = join(logDir, `${s.name}-edge.json`);
  writeFileSync(cfgPath, JSON.stringify(edgeConfig(s, relayUrl, build.legacy), null, 2));
  const edgeBin = join(build.binDir, build.legacy ? 'tq-publisher' : 'tq-edge');
  const edge = spawn([edgeBin, '--config', cfgPath, '--relay-url', relayUrl], { log: join(logDir, `${s.name}-edge.log`) });
  await Bun.sleep(3000); // connected, first keyframes through

  const cpu0 = { relay: relay && (await cpuSecs(relay.pid)), edge: await cpuSecs(edge.pid), t: performance.now() };
  const result: any = { name: s.name, what: s.what, secs };
  const plog = (i: number | string) => join(logDir, `${s.name}-probe-${i}.log`);

  if (s.kind === 'video') {
    const appRun = s.app ? runApp(build, secs, join(logDir, `${s.name}-app.log`)) : null;
    const r = await probe(build, ['--preset', 'operator', '--teleop-hz', '20', ...liveArg(s, build)], secs + (build.legacy ? 0 : WARMUP_SECS), plog(0));
    result.tracks = videoTracks(r);
    result.teleop = {
      ack_rtt_ms: pick(r.ack_rtt_ms),
      heartbeat_rtt_ms: pick(r.heartbeat_rtt_ms),
      acks: r.acks,
      tripped_at_end: r.deadman_tripped,
    };
    result.clock_check = r.clock_check ?? null;
    result.quic = r.quic;
    if (appRun) result.app = await appRun;
  } else if (s.kind === 'data') {
    const r = await probe(build, ['--preset', 'viewer'], secs, plog(0));
    result.tracks = Object.fromEntries(
      Object.entries<any>(r.tracks)
        .filter(([p]) => p.includes('/data-'))
        .map(([p, t]) => [p, { expect_hz: DATA_HZ[p.split('data-')[1] as keyof typeof DATA_HZ], fps: t.fps, edge_to_probe_ms: pick(t.capture_to_probe_ms), pub_to_relay_ms: pick(t.publisher_to_relay_ms), relay_to_probe_ms: pick(t.relay_to_probe_ms) }]),
    );
    result.clock_check = r.clock_check ?? null;
  } else if (s.kind === 'join') {
    // Cold: nobody subscribes, so tq-edge sends nothing until we join.
    // Warm: a viewer is already in; the relay has the group cached.
    const joins: Record<string, Record<string, { first: number[]; live: number[] }>> = {};
    const once = async (mode: string, i: number) => {
      const r = await probe(build, ['--preset', 'viewer', ...liveArg(s, build)], secs, plog(`${mode}-${i}`));
      for (const [p, t] of Object.entries<any>(r.tracks ?? {})) {
        if (t.kind !== 'video') continue;
        const j = ((joins[mode] ??= {})[p] ??= { first: [], live: [] });
        if (t.join?.first_frame_ms != null) j.first.push(t.join.first_frame_ms);
        if (t.join?.live_ms != null) j.live.push(t.join.live_ms);
      }
    };
    for (let i = 0; i < JOINS; i++) {
      await once('cold', i);
      await Bun.sleep(1500); // the edge notices nobody is left and stops
    }
    const holder = Bun.spawn([join(build.binDir, 'tq-probe'), '--relay', RELAY, '--cert-url', CERT_URL, '--duration', '0'], {
      env: { ...process.env, TQ_SHARED_SECRET: SECRET, RUST_LOG: 'warn' },
      stdout: 'ignore',
      stderr: 'ignore',
    });
    procs.push(holder);
    await Bun.sleep(2000);
    for (let i = 0; i < JOINS; i++) await once('warm', i);
    holder.kill();
    result.live_threshold_ms = liveMs(s);
    result.join = Object.fromEntries(
      Object.entries(joins).map(([mode, tracks]) => [
        mode,
        Object.fromEntries(
          Object.entries(tracks).map(([p, j]) => [p, { first_frame_ms: summarize(j.first), live_ms: summarize(j.live), live_of: `${j.live.length}/${JOINS}` }]),
        ),
      ]),
    );
  } else if (s.kind === 'stop') {
    const rtts: number[] = [];
    const statuses: Record<string, number> = {};
    const sends: number[] = []; // copies sent per stop until one was acked
    const runs = 10;
    for (let i = 0; i < runs; i++) {
      // Resent every 50 ms until acked, as the consoles do; 4 s outlasts
      // the probe's 3 s wait, so a stop never acked shows as no_ack.
      const r = await probe(build, ['--preset', 'operator', '--send', 'robot-01/stop={}', '--resend-ms', '50'], 4, plog(i));
      for (const x of r.sends ?? []) {
        statuses[x.status] = (statuses[x.status] ?? 0) + 1;
        if (typeof x.rtt_ms === 'number') rtts.push(x.rtt_ms);
        sends.push(x.sends ?? 1);
      }
    }
    // The remote stop must always stop the robot: anything but an Ok ack
    // fails the scenario rather than leaving a latency of the ones that did.
    const bad = Object.entries(statuses).filter(([status]) => status !== 'Ok');
    if (bad.length || rtts.length < runs) {
      throw new Error(`remote stop not acked Ok every time: ${JSON.stringify(statuses)}, ${rtts.length}/${runs} with a round trip`);
    }
    result.stop_ack_rtt_ms = summarize(rtts);
    result.statuses = statuses;
    result.sends_per_stop = summarize(sends);
  } else if (s.kind === 'fanout') {
    const n = s.probes ?? 10;
    const rs = await Promise.all(Array.from({ length: n }, (_, i) => probe(build, ['--preset', 'viewer'], secs, plog(i))));
    const per = (track: string, key: string, q: 'p50' | 'p95') =>
      summarize(rs.map((r) => r.tracks?.[track]?.[key]?.[q]).filter((x) => typeof x === 'number'));
    result.probes = n;
    result.tracks = Object.fromEntries(
      Object.keys(rs[0]?.tracks ?? {})
        .filter((t) => rs[0].tracks[t].kind === 'video')
        .map((t) => [
          t,
          {
            fps_min: Math.min(...rs.map((r) => r.tracks?.[t]?.fps ?? 0)),
            capture_to_probe_p50_across_probes: per(t, 'capture_to_probe_ms', 'p50'),
            capture_to_probe_p95_across_probes: per(t, 'capture_to_probe_ms', 'p95'),
          },
        ]),
    );
  }

  const elapsed = (performance.now() - cpu0.t) / 1000;
  const cpu = async (pid: number, c0: number | null) => {
    const c1 = await cpuSecs(pid);
    return c0 != null && c1 != null ? Math.round(((c1 - c0) / elapsed) * 1000) / 10 : null;
  };
  result.cpu_pct = { relay: relay && (await cpu(relay.pid, cpu0.relay)), edge: await cpu(edge.pid, cpu0.edge) };
  await stopAll();
  return result;
}

function videoTracks(r: any) {
  return Object.fromEntries(
    Object.entries<any>(r.tracks ?? {})
      .filter(([, t]) => t.kind === 'video')
      .map(([p, t]) => [
        p,
        {
          codec: t.codec,
          fps: t.fps,
          kbps: t.kbps,
          capture_to_probe_ms: pick(t.capture_to_probe_ms),
          capture_to_pub_ms: pick(t.capture_to_publisher_ms),
          pub_to_relay_ms: pick(t.publisher_to_relay_ms),
          relay_to_probe_ms: pick(t.relay_to_probe_ms),
          over_50ms: t.over_50ms,
          over_100ms: t.over_100ms,
        },
      ]),
  );
}

function liveArg(s: Scenario, build: Build) {
  return build.legacy ? [] : ['--live-threshold-ms', String(liveMs(s))];
}

function summarize(v: number[]): Summary {
  if (!v.length) return null;
  const s = [...v].sort((a, b) => a - b);
  const at = (p: number) => s[Math.round((s.length - 1) * p)];
  const r = (x: number) => Math.round(x * 10) / 10;
  return { n: s.length, mean: r(s.reduce((a, b) => a + b, 0) / s.length), p50: r(at(0.5)), p95: r(at(0.95)), max: r(at(1)) };
}

/** tq-app --log-stats alongside the probe: decode and paint in the real app. */
async function runApp(build: Build, secs: number, log: string) {
  const bin = join(build.binDir, 'tq-app');
  if (!existsSync(bin)) return { skipped: 'no tq-app binary (cargo build --release -p tq-app)' };
  const p = Bun.spawn([bin, '--relay-http', RELAY_HTTP, build.legacy ? '--role' : '--preset', 'viewer', '--log-stats'], {
    env: { ...process.env, TQ_SHARED_SECRET: SECRET, RUST_LOG: 'info' },
    stdout: 'pipe',
    stderr: 'pipe',
  });
  procs.push(p);
  setTimeout(() => p.kill(), secs * 1000);
  const [out, err] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text()]);
  const text = (out + err).replace(/\x1b\[[0-9;]*m/g, '');
  writeFileSync(log, text);
  // "robot-01/front: ... | capture→decoded  24.8/28.1 | decoded→painted  6.0/17.8 | capture→render 30.2/45.8 ..."
  const per: Record<string, Record<string, { avg: number[]; max: number[] }>> = {};
  for (const line of text.split('\n')) {
    const m = line.match(/ui::model: (\S+): /);
    if (!m) continue;
    // tq-app 0.4 (protocol 2.0) names cameras without the broadcast.
    if (!m[1].includes('/')) m[1] = `robot-01/${m[1]}`;
    for (const [, key, avg, max] of line.matchAll(/\| (capture→decoded|decoded→painted|capture→render|decode)\s+([\d.]+)\/([\d.]+)/g)) {
      const k = ((per[m[1]] ??= {})[key] ??= { avg: [], max: [] });
      k.avg.push(Number(avg));
      k.max.push(Number(max));
    }
  }
  // The first windows include the connect and the first keyframe.
  return Object.fromEntries(
    Object.entries(per).map(([t, keys]) => [
      t,
      Object.fromEntries(
        Object.entries(keys).map(([k, v]) => [k, { avg_of_2s_avgs: summarize(v.avg.slice(2))?.mean ?? null, worst_2s_max: Math.max(...v.max.slice(2)) }]),
      ),
    ]),
  );
}

async function run(args: Map<string, string | true>) {
  const remote = args.get('relay');
  if (typeof remote === 'string') {
    // tq-edge, the sources and the probes stay on this machine, so
    // capture→probe is still on one clock and clock_check still holds;
    // only the relay is elsewhere. The link is whatever the internet is,
    // so the emulated-link scenarios are skipped.
    const u = new URL(remote);
    const secret = process.env.TQ_SHARED_SECRET;
    if (!secret) throw new Error('--relay needs TQ_SHARED_SECRET (the relay\'s token secret)');
    const info = (await (await fetch(new URL('/api/info', u))).json()) as any;
    REMOTE = u.origin;
    SECRET = secret;
    RELAY_HTTP = u.origin;
    CERT_URL = new URL('/api/cert', u).href;
    RELAY = `https://${u.hostname}:${info.quic_port}`;
    console.error(`relay ${RELAY}: ${info.build?.component} ${info.build?.version} (${info.build?.git_sha})`);
  }
  const build = await prepareBuild(args);
  const quick = args.has('quick');
  const only = typeof args.get('only') === 'string' ? (args.get('only') as string).split(',') : null;
  const chosen = SCENARIOS.filter((s) => (only ? only.includes(s.name) : !quick || s.quick)).filter(
    (s) => !(build.legacy && s.v3Only) && !(REMOTE && s.impair) && (!s.app || args.has('app')),
  );
  if (!existsSync(join(STREAMING_TOOLS, 'tools/gstreamer_source'))) throw new Error(`no streaming-tools at ${STREAMING_TOOLS}`);
  const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  const outFile = (args.get('out') as string) ?? join(ROOT, 'target/bench', `${stamp}-${build.label}.json`);
  const logDir = join(dirname(outFile), `${stamp}-${build.label}-logs`);
  mkdirSync(logDir, { recursive: true });
  if (args.has('save-baseline') && (quick || only)) throw new Error('--save-baseline needs a full run (no --quick or --only)');
  // The hardware model, not the host name: baselines are committed.
  const machine = (
    await sh(['sh', '-c', 'echo "$(uname -sm) $(sysctl -n hw.model 2>/dev/null) $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 "model name" /proc/cpuinfo | cut -d: -f2)"'])
  )
    .replace(/\s+/g, ' ')
    .trim();
  const out: any = {
    bench: 1,
    label: build.label,
    commit: build.commit,
    protocol: build.legacy ? '2' : '3',
    relay: REMOTE ?? 'local',
    machine,
    started: new Date().toISOString(),
    quick,
    scenarios: {},
  };
  for (const s of chosen) {
    const secs = quick ? s.quickSecs : s.secs;
    console.error(`[${s.name}] ${s.what} (${secs} s)`);
    try {
      out.scenarios[s.name] = await runScenario(s, build, secs, logDir);
    } catch (e) {
      await stopAll();
      console.error(`[${s.name}] failed: ${e}`);
      out.scenarios[s.name] = { name: s.name, error: String(e) };
    }
    writeFileSync(outFile, JSON.stringify(out, null, 2));
  }
  console.error(`\nwrote ${outFile}\nlogs in ${logDir}`);
  if (args.has('save-baseline')) {
    const failed = Object.values<any>(out.scenarios).filter((x) => x.error);
    if (failed.length) throw new Error(`not saving a baseline: ${failed.map((x) => x.name).join(', ')} failed`);
    const file = baselinePath(out);
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, JSON.stringify(out, null, 2) + '\n');
    console.error(`baseline ${file}`);
  }
  console.log(report(out));
}

// ---------------------------------------------------------------- report / compare

function fmt(s: Summary | { p50: number; p95: number } | null | undefined) {
  return s ? `${s.p50} / ${s.p95}` : '–';
}

function report(r: any): string {
  const where = r.relay && r.relay !== 'local' ? `, relay ${r.relay}` : '';
  const lines = [`# bench ${r.label} (protocol ${r.protocol}) on ${r.machine}${where}${r.quick ? ', quick' : ''}`, ''];
  for (const s of Object.values<any>(r.scenarios)) {
    lines.push(`## ${s.name}${s.error ? ': FAILED ' + s.error : ''}`);
    if (s.error) continue;
    if (s.tracks) {
      lines.push('| track | fps | capture→probe p50/p95 | capture→pub | pub→relay | relay→probe | >50 / >100 ms |', '| --- | --- | --- | --- | --- | --- | --- |');
      for (const [p, t] of Object.entries<any>(s.tracks)) {
        lines.push(
          `| ${p} | ${t.fps ?? t.fps_min} | ${fmt(t.capture_to_probe_ms ?? t.edge_to_probe_ms ?? t.capture_to_probe_p50_across_probes)} | ${fmt(t.capture_to_pub_ms)} | ${fmt(t.pub_to_relay_ms)} | ${fmt(t.relay_to_probe_ms)} | ${t.over_50ms ?? '–'} / ${t.over_100ms ?? '–'} |`,
        );
      }
    }
    if (s.teleop) lines.push(`teleop ack rtt ${fmt(s.teleop.ack_rtt_ms)} · heartbeat ${fmt(s.teleop.heartbeat_rtt_ms)} · acks ${JSON.stringify(s.teleop.acks)}`);
    if (s.join) {
      for (const [mode, tracks] of Object.entries<any>(s.join)) {
        for (const [p, j] of Object.entries<any>(tracks)) {
          lines.push(`join ${mode} ${p}: first frame ${fmt(j.first_frame_ms)} ms, live (≤${s.live_threshold_ms} ms) ${fmt(j.live_ms)} ms (${j.live_of})`);
        }
      }
    }
    if (s.stop_ack_rtt_ms !== undefined) lines.push(`stop ack rtt ${fmt(s.stop_ack_rtt_ms)} · ${JSON.stringify(s.statuses)} · sends per stop ${fmt(s.sends_per_stop)}`);
    if (s.clock_check) {
      for (const [p, c] of Object.entries<any>(s.clock_check)) {
        lines.push(`clock ${p}: |error| ${fmt(c.abs_error_ms)} ms, claimed bound ${fmt(c.claimed_bound_ms)} ms, within bound ${(c.within_bound * 100).toFixed(1)}%`);
      }
    }
    if (s.app) lines.push(`tq-app: ${JSON.stringify(s.app)}`);
    lines.push(`cpu relay ${s.cpu_pct?.relay}% edge ${s.cpu_pct?.edge}%`, '');
  }
  return lines.join('\n');
}

/** Every latency number in a result, flattened: "scenario track metric q" → value. */
function metrics(r: any): Map<string, number> {
  const m = new Map<string, number>();
  const put = (k: string, s: any) => {
    if (!s) return;
    if (typeof s.p50 === 'number') m.set(`${k} p50`, s.p50);
    if (typeof s.p95 === 'number') m.set(`${k} p95`, s.p95);
  };
  for (const s of Object.values<any>(r.scenarios)) {
    if (s.error) continue;
    for (const [p, t] of Object.entries<any>(s.tracks ?? {})) {
      for (const key of ['capture_to_probe_ms', 'capture_to_pub_ms', 'pub_to_relay_ms', 'relay_to_probe_ms', 'edge_to_probe_ms', 'capture_to_probe_p50_across_probes', 'capture_to_probe_p95_across_probes']) {
        put(`${s.name} ${p} ${key}`, t[key]);
      }
    }
    put(`${s.name} teleop ack_rtt_ms`, s.teleop?.ack_rtt_ms);
    put(`${s.name} stop_ack_rtt_ms`, s.stop_ack_rtt_ms);
    for (const [t, keys] of Object.entries<any>(s.app ?? {})) {
      if (typeof keys !== 'object' || keys === null) continue;
      for (const [k, v] of Object.entries<any>(keys)) {
        if (typeof v?.avg_of_2s_avgs === 'number') m.set(`${s.name} app ${t} ${k} avg`, v.avg_of_2s_avgs);
      }
    }
    for (const [mode, tracks] of Object.entries<any>(s.join ?? {})) {
      for (const [p, j] of Object.entries<any>(tracks)) {
        put(`${s.name} ${mode} ${p} first_frame_ms`, j.first_frame_ms);
        put(`${s.name} ${mode} ${p} live_ms`, j.live_ms);
      }
    }
  }
  return m;
}

/**
 * A change is a regression past both a relative and an absolute margin,
 * each sized to what the metric does between two runs of the same build
 * (docs/perf.md, "Noise").
 */
function margin(key: string, remote: boolean): { rel: number; abs: number } {
  // Where in the 1 s GOP a join lands: medians of 10 joins move ~150 ms.
  if (/ (first_frame|live)_ms /.test(key)) return { rel: 0.5, abs: 250 };
  // tq-app paints on the display link: 0–16.7 ms of wait per frame.
  if (/ app .*(painted|render)/.test(key)) return { rel: 0.3, abs: 10 };
  const p95 = key.endsWith('p95');
  // The internet's tail moves more than loopback's.
  if (p95) return { rel: 0.25, abs: remote ? 10 : 5 };
  return { rel: 0.15, abs: 3 };
}

function regressed(key: string, base: number, now: number, remote = false) {
  const m = margin(key, remote);
  return now - base > Math.max(base * m.rel, m.abs);
}

function compare(a: any, b: any): { text: string; regressions: number } {
  const ma = metrics(a);
  const mb = metrics(b);
  const lines = [`# ${a.label} (protocol ${a.protocol}) → ${b.label} (protocol ${b.protocol})`, ''];
  if ((a.relay ?? 'local') !== (b.relay ?? 'local')) lines.push(`**different relays** (${a.relay ?? 'local'} vs ${b.relay ?? 'local'})`, '');
  if (a.machine !== b.machine) lines.push(`**different machines** (${a.machine} vs ${b.machine}): numbers are not comparable`, '');
  lines.push('| metric (ms) | base | new | Δ | |', '| --- | --- | --- | --- | --- |');
  let regressions = 0;
  for (const [k, v] of ma) {
    if (!mb.has(k)) continue;
    const n = mb.get(k)!;
    const remote = (b.relay ?? 'local') !== 'local';
    const bad = regressed(k, v, n, remote);
    const better = regressed(k, n, v, remote);
    regressions += Number(bad);
    lines.push(`| ${k} | ${v} | ${n} | ${n - v > 0 ? '+' : ''}${(n - v).toFixed(1)} | ${bad ? 'REGRESSED' : better ? 'better' : ''} |`);
  }
  // Metrics on one side only, by scenario (new scenarios, or ones the base cannot run).
  const only = (x: Map<string, number>, y: Map<string, number>) => {
    const by = new Map<string, number>();
    for (const k of x.keys()) if (!y.has(k)) by.set(k.split(' ')[0], (by.get(k.split(' ')[0]) ?? 0) + 1);
    return [...by].map(([s, n]) => `${s} (${n})`).join(', ');
  };
  if (only(mb, ma)) lines.push('', `new only: ${only(mb, ma)}`);
  if (only(ma, mb)) lines.push('', `base only: ${only(ma, mb)}`);
  for (const [name, s] of Object.entries<any>(b.scenarios ?? {})) {
    if (s.error && !a.scenarios?.[name]?.error) {
      regressions++;
      lines.push('', `**${name} FAILED in new**: ${s.error}`);
    }
  }
  lines.push('', `${regressions} regression(s)`);
  return { text: lines.join('\n'), regressions };
}

// ---------------------------------------------------------------- baselines

/** dev/bench/baselines/<machine>/<local | relay host>.json: one per machine and relay, replaced on each release. */
function baselinePath(r: any): string {
  const slug = (x: string) => x.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');
  const relay = r.relay && r.relay !== 'local' ? new URL(r.relay).hostname : 'local';
  return join(ROOT, 'dev/bench/baselines', slug(r.machine), `${relay}.json`);
}

// ---------------------------------------------------------------- main

function parseArgs(argv: string[]) {
  const m = new Map<string, string | true>();
  const pos: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('--')) pos.push(a);
    else if (argv[i + 1] && !argv[i + 1].startsWith('--') && !['quick', 'app', 'no-build', 'fail', 'save-baseline'].includes(a.slice(2))) m.set(a.slice(2), argv[++i]);
    else m.set(a.slice(2), true);
  }
  return { m, pos };
}

const [cmd, ...rest] = process.argv.slice(2);
const { m, pos } = parseArgs(rest);
if (cmd === 'run') {
  await run(m);
} else if (cmd === 'compare' && (pos.length === 1 || pos.length === 2)) {
  let files = pos;
  if (pos.length === 1) {
    const base = baselinePath(JSON.parse(readFileSync(pos[0], 'utf8')));
    if (!existsSync(base)) {
      console.error(`no baseline for this machine and relay: ${base}\n(run --save-baseline on a release to make one)`);
      process.exit(2);
    }
    files = [base, pos[0]];
  }
  const [a, b] = files.map((p) => JSON.parse(readFileSync(p, 'utf8')));
  const { text, regressions } = compare(a, b);
  console.log(text);
  if (m.has('fail') && regressions) process.exit(1);
} else if (cmd === 'report' && pos.length === 1) {
  console.log(report(JSON.parse(readFileSync(pos[0], 'utf8'))));
} else if (cmd === 'list') {
  for (const s of SCENARIOS) console.log(`${s.quick ? '*' : ' '} ${s.name.padEnd(22)} ${s.secs}s  ${s.what}${s.app ? ' [--app]' : ''}`);
  console.log('\n* in --quick; [--app] only with --app');
} else {
  console.error('usage: bun dev/bench/bench.ts run [--quick] [--only a,b] [--app] [--ref REF | --bin-dir DIR] [--relay URL] [--save-baseline] [--label L] [--out F]\n       bun dev/bench/bench.ts compare [BASE.json] NEW.json [--fail]\n       bun dev/bench/bench.ts report RESULT.json\n       bun dev/bench/bench.ts list');
  process.exit(2);
}
