// Operator / viewer page. The media worker (worker.ts) does all transport,
// decoding and drawing; this thread only builds the UI, forwards operator
// input and shows the numbers.

import {
  type BroadcastState,
  type BuildInfo,
  type Control,
  type ParticipantInfo,
  type Path,
  type RobotStatus,
  type RoomEventBody,
  type RoomMessage,
  type RoomState,
  type TrackInfo,
  compat,
  mismatch,
  path,
  shortBuild,
} from './protocol';
import { WEB_BUILD } from './version';
import type { CameraStats, FromWorker, ToWorker, Window } from './worker';

/** Set by build.sh from TQ_RELAY_HTTP; empty when the relay serves the page. */
declare const TQ_RELAY_HTTP: string;

type Preset = 'operator' | 'viewer';

const params = new URLSearchParams(location.search);
const room = params.get('room') ?? 'robot-01';
// `role=` is what links from before protocol 3.0 say.
const preset: Preset = (params.get('preset') ?? params.get('role')) === 'operator' ? 'operator' : 'viewer';
// Photons -> capture timestamp (exposure, readout, ISP/USB) that the
// camera's timestamp does not cover; calibrate with an optical measurement.
const sensorMs = Number(params.get('sensor_ms') ?? 0) || 0;

/** The teleop lock and channels (crates/tq-teleop). */
const DRIVE = 'drive';
const ESTOP = 'estop';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const el = <K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

const worker = new Worker('/worker.js', { type: 'module' });
const send = (msg: ToWorker, transfer: Transferable[] = []) => worker.postMessage(msg, transfer);

let session: string | null = null;
let state: RoomState | null = null;
let relayBuild: BuildInfo | null = null;
const ownersSeen = new Map<string, string>();
/** Each broadcast's latest `.status` message. */
const statuses = new Map<string, RobotStatus>();
const tiles = new Map<Path, { root: HTMLElement; overlay: HTMLElement; label: HTMLElement }>();

const fmt = (v: number | null | undefined, digits = 0) => (v === null || v === undefined ? '–' : v.toFixed(digits));
const win = (w: Window) => (w.avg === null ? '–' : `${w.avg.toFixed(1)}/${w.max!.toFixed(1)}`);

// The broadcast the operator drives: the first with a drive lock.
function driveTarget(): BroadcastState | null {
  return state?.broadcasts.find((b) => b.locks.some((l) => l.info.id === DRIVE)) ?? null;
}

// --- tiles -----------------------------------------------------------------

const tileLabel = (p: Path, info: TrackInfo) => (info.label ? `${info.label} · ${p}` : p);

// A tile per video track of every broadcast; tracks that are gone lose
// theirs. (Called with a state only: a disconnect keeps the tiles.)
function syncTiles(s: RoomState) {
  const grid = $('grid');
  const live = new Set<Path>();
  for (const b of s.broadcasts) {
    for (const t of b.tracks) {
      if (t.info.kind !== 'video') continue;
      const p = path(b.name, t.info.id);
      live.add(p);
      const existing = tiles.get(p);
      if (existing) {
        existing.label.textContent = tileLabel(p, t.info);
        existing.root.classList.toggle('offline', !t.online);
        continue;
      }
      const root = el('div', 'tile');
      const canvas = el('canvas');
      const label = el('div', 'label', tileLabel(p, t.info));
      const overlay = el('pre', 'overlay', 'waiting for keyframe…');
      const key = el('button', 'small', 'keyframe');
      key.title = 'Ask the broadcast owner for a keyframe';
      key.onclick = () => send({ type: 'control', msg: { type: 'keyframe_request', track: p, reason: 'user' } });
      const bar = el('div', 'tilebar');
      bar.append(label, key);
      root.append(canvas, bar, overlay);
      root.classList.toggle('offline', !t.online);
      grid.append(root);
      const offscreen = canvas.transferControlToOffscreen();
      send({ type: 'canvas', track: p, canvas: offscreen }, [offscreen]);
      tiles.set(p, { root, overlay, label });
    }
  }
  for (const [p, tile] of tiles) {
    if (live.has(p)) continue;
    tile.root.remove();
    tiles.delete(p);
  }
}

function renderCameraStats(p: Path, s: CameraStats) {
  const tile = tiles.get(p);
  if (!tile) return;
  // Capture times are the owner's receive time without the encoder's SEI.
  const sei = s.captureFromSei ? '' : ' (no SEI: capture = publisher time)';
  tile.overlay.textContent = [
    `${s.codec ?? '–'} ${s.size}${s.spsPatched ? ' sps-patched' : ''}`,
    `${fmt(s.fps, 1)} fps  ${fmt(s.kbps)} kbps  q=${s.decodeQueue}`,
    `frames ${s.frames}  drop ${s.dropped}  gops ${s.groups}  err ${s.errors}`,
    `ms avg/max${sei}`,
    `  capture→pub   ${win(s.captureToPublisher)}`,
    `  pub→relay     ${win(s.publisherToRelay)}`,
    `  relay→browser ${win(s.relayToBrowser)}`,
    `  decode        ${win(s.decode)}`,
    `  capture→drawn ${win(s.captureToDrawn)}`,
    `  draw→display≈ ${win(s.drawToDisplay)}`,
    `  glass→glass≈  ${win(s.glassToGlass)}  ±${fmt(s.syncErrorMs, 1)} sync${sensorMs ? ` +${sensorMs} sensor` : ''}`,
  ].join('\n');
  tile.root.classList.toggle('stale', s.fps < 0.5);
}

// --- versions ----------------------------------------------------------------

const describe = (b: BuildInfo | null | undefined) =>
  b ? `${shortBuild(b)}, protocol ${b.protocol.major}.${b.protocol.minor}` : 'unknown (protocol 1.0)';

// "web 0.1.0 · relay 0.1.0 · rover 0.1.0"; amber when a protocol minor
// differs (works, but one side may lack features). Hover for commits.
function renderVersions() {
  const parts = [`web ${WEB_BUILD.version}`];
  const title = [`web ${describe(WEB_BUILD)}`];
  let warn = false;
  if (session) {
    parts.push(`relay ${relayBuild?.version ?? '?'}`);
    title.push(`relay ${describe(relayBuild)}`);
    warn ||= compat(relayBuild) !== 'same';
  }
  for (const b of state?.broadcasts ?? []) {
    if (!b.owner) continue;
    parts.push(`${b.name} ${b.build?.version ?? '?'}`);
    title.push(`${b.name}: ${b.build?.component ?? 'owner'} ${describe(b.build)}`);
    warn ||= compat(b.build) !== 'same';
    const line = `${b.name} is published by ${b.build?.component ?? 'a session'} ${describe(b.build)}`;
    if (ownersSeen.get(b.name) !== line) {
      ownersSeen.set(b.name, line);
      log(line);
      if (compat(b.build) !== 'same') log(`warning: ${mismatch(b.name, b.build)}`);
    }
  }
  const pill = $('versions');
  pill.textContent = parts.join(' · ');
  pill.title = title.join('\n');
  pill.className = `pill ${warn ? 'warn' : ''}`;
}

// --- room state ------------------------------------------------------------

// What a participant may do, from its permissions.
function kind(p: ParticipantInfo): 'publisher' | 'operator' | 'viewer' {
  if (p.perms.some((x) => x.startsWith('pub:'))) return 'publisher';
  if (p.perms.some((x) => x.startsWith('lock:') || x.startsWith('write:'))) return 'operator';
  return 'viewer';
}

function renderDeadman(b: BroadcastState | null) {
  const t = b?.owner ? statuses.get(b.name)?.teleop : undefined;
  const dm = $('deadman');
  if (!t) {
    dm.textContent = 'dead-man: –';
    dm.className = 'deadman';
    return;
  }
  dm.textContent = t.tripped ? `SAFE-STOPPED: ${t.reason}` : `ARMED · ${who(t.holder)}`;
  dm.className = `deadman ${t.tripped ? 'tripped' : 'armed'}`;
}

function renderState() {
  if (!state) {
    // Disconnected: nothing room-dependent is known until the next state.
    $('claim-status').textContent = '–';
    $('claim-status').className = 'pill';
    document.body.classList.remove('driving');
    $('publisher').textContent = 'no broadcasts';
    $('publisher').className = 'pill bad';
    renderDeadman(null);
    $('viewers').textContent = '';
    renderVersions();
    return;
  }
  syncTiles(state);
  for (const name of statuses.keys()) if (!state.broadcasts.some((b) => b.name === name)) statuses.delete(name);

  const target = driveTarget();
  const lock = target?.locks.find((l) => l.info.id === DRIVE);
  const holder = lock?.holder ?? null;
  const mine = holder !== null && holder === session;
  $('claim-status').textContent = !target
    ? 'no drive lock'
    : holder === null
      ? `${target.name}: free`
      : mine
        ? `you are driving ${target.name}`
        : `${target.name}: held by ${who(holder)}`;
  $('claim-status').className = `pill ${mine ? 'ok' : holder ? 'warn' : ''}`;
  document.body.classList.toggle('driving', mine);

  const pub = $('publisher');
  if (!state.broadcasts.length) {
    pub.textContent = 'no broadcasts';
    pub.className = 'pill bad';
  } else {
    pub.textContent = state.broadcasts
      .map((b) =>
        b.owner
          ? `${b.name} · rtt ${fmt(b.rtt_ms, 1)} ms · clock ${fmt(b.clock_offset_us === null ? null : b.clock_offset_us / 1000, 1)} ms`
          : `${b.name} offline`,
      )
      .join('  |  ');
    const offline = state.broadcasts.filter((b) => !b.owner).length;
    pub.className = `pill ${offline === 0 ? 'ok' : offline === state.broadcasts.length ? 'bad' : 'warn'}`;
  }
  renderDeadman(target);
  const count = (k: ReturnType<typeof kind>) => state!.participants.filter((x) => kind(x) === k).length;
  $('viewers').textContent = `${count('operator')} operator(s), ${count('viewer')} viewer(s), ${count('publisher')} publisher(s)`;
  renderVersions();

  const rows: string[] = [];
  for (const b of state.broadcasts) {
    if (b.quic) {
      rows.push(
        `${b.name} uplink quic: rtt ${b.quic.rtt_ms.toFixed(1)} ms  cwnd ${(b.quic.cwnd / 1024).toFixed(0)} KiB  lost ${b.quic.lost_packets}/${b.quic.sent_packets}  cong ${b.quic.congestion_events}`,
      );
    }
    for (const t of b.tracks) {
      const s = t.stats;
      rows.push(
        `${path(b.name, t.info.id)}: ${t.online ? 'online' : 'OFFLINE'} ${t.info.kind} ${s.codec_string ?? ''} ${s.fps.toFixed(1)} fps ${s.kbps.toFixed(0)} kbps  uplink drops ${s.dropped}  capture→pub ${fmt(s.capture_to_publisher_ms, 1)} ms  subs ${s.subscribers}`,
      );
    }
  }
  $('publisher-detail').textContent = rows.join('\n');
}

function onControl(msg: Control) {
  switch (msg.type) {
    case 'welcome':
      session = msg.session;
      relayBuild = msg.relay ?? null;
      log(`relay runs ${describe(relayBuild)}`);
      if (compat(relayBuild) !== 'same') log(`warning: ${mismatch('relay', relayBuild)}`);
      $('session').textContent = `${preset} ${msg.session.slice(0, 8)}`;
      $('session').title = msg.perms.join('\n');
      renderState();
      break;
    case 'claim_result':
      if (!msg.ok && msg.reason) log(`claim of ${msg.lock} refused: ${msg.reason}`);
      break;
    case 'subscribe_result':
      if (!msg.ok) log(`subscribe ${msg.track} refused: ${msg.reason ?? ''}`);
      break;
    case 'link_stats':
      $('link').textContent = `downlink quic: rtt ${msg.quic.rtt_ms.toFixed(1)} ms  cwnd ${(msg.quic.cwnd / 1024).toFixed(0)} KiB  lost ${msg.quic.lost_packets}/${msg.quic.sent_packets}`;
      break;
    case 'error':
      log(`relay error: ${msg.message}`);
      break;
    default:
      break;
  }
}

// A session as the log shows it: "you", or who and which session.
function who(id: string | null): string {
  if (id === null) return '–';
  if (id === session) return 'you';
  const p = state?.participants.find((x) => x.session === id);
  return p ? `${p.sub} (${id.slice(0, 8)})` : id.slice(0, 8);
}

function describeEvent(e: RoomEventBody): string | null {
  switch (e.type) {
    case 'participant_joined':
      if (e.participant.session === session) return null;
      return `${e.participant.sub} (${e.participant.session.slice(0, 8)}) joined as ${kind(e.participant)}`;
    case 'participant_left':
      return `${e.participant.sub} (${e.participant.session.slice(0, 8)}) left`;
    case 'broadcast_started':
      return `${e.broadcast} started by ${who(e.owner)}${e.build ? ` (${e.build.component} ${e.build.version})` : ''}`;
    case 'broadcast_stopped':
      return `${e.broadcast} stopped: ${e.reason === 'replaced' ? 'replaced by a new session' : e.reason}`;
    case 'broadcast_removed':
      return `${e.broadcast} removed`;
    case 'track_published':
      return `${e.info.kind} track ${e.track} published`;
    case 'track_updated': {
      const v = e.info.video;
      return `track ${e.track} changed${v ? `: ${v.codec} ${v.width}x${v.height}` : ''}`;
    }
    case 'track_unpublished':
      return `track ${e.track} unpublished`;
    case 'track_online':
      return `track ${e.track} online`;
    case 'track_offline':
      return `track ${e.track} offline: ${e.reason}`;
    case 'lock_changed':
      switch (e.reason) {
        case 'claimed':
          return `${who(e.to)} took ${e.lock}`;
        case 'forced':
          return `${who(e.to)} took ${e.lock} from ${who(e.from)}`;
        case 'released':
          return `${who(e.from)} released ${e.lock}`;
        case 'left':
          return `${e.lock} freed: ${who(e.from)} left`;
      }
      return null;
    default:
      return null;
  }
}

function onRoom(msg: RoomMessage) {
  if (msg.type === 'state') {
    state = msg.state;
    renderState();
  } else if (msg.type === 'event') {
    const line = describeEvent(msg.event);
    if (line) log(line);
  }
}

// A message on a broadcast's data track: `.status` feeds the dead-man
// display, `.events` goes to the log.
function onData(track: Path, value: unknown, atUs: number) {
  const [broadcast, name] = track.split('/') as [string, string];
  if (name === '.status') {
    const prev = statuses.get(broadcast)?.teleop;
    const next = value as RobotStatus;
    statuses.set(broadcast, next);
    if (prev && next.teleop && prev.tripped !== next.teleop.tripped) {
      log(next.teleop.tripped ? `${broadcast} safe-stopped: ${next.teleop.reason}` : `${broadcast} armed`);
    }
    renderDeadman(driveTarget());
  } else if (name === '.events') {
    log(`${broadcast}: ${JSON.stringify(value)}`, atUs / 1000);
  }
}

// Newest first. A line with a time (Unix ms) goes after any newer ones: a
// broadcast's `.events` replayed on subscribing can be older than lines
// before it.
const logLines: { atMs: number; text: string }[] = [];
function log(line: string, atMs = Date.now()) {
  const i = logLines.findIndex((l) => l.atMs <= atMs);
  logLines.splice(i < 0 ? logLines.length : i, 0, { atMs, text: `${new Date(atMs).toLocaleTimeString()} ${line}` });
  logLines.length = Math.min(logLines.length, 100);
  $('log').textContent = logLines.map((l) => l.text).join('\n');
}

// --- teleop ----------------------------------------------------------------

const keys = new Set<string>();
let streaming = false;
let lastSent = '';

function driveCommand() {
  const throttle = (keys.has('w') ? 1 : 0) - (keys.has('s') ? 1 : 0);
  const steer = (keys.has('d') ? 1 : 0) - (keys.has('a') ? 1 : 0);
  return { throttle, steer };
}

function sendDrive() {
  const target = driveTarget();
  if (!target || !target.channels.some((c) => c.info.id === DRIVE)) return;
  const args = driveCommand();
  send({ type: 'message', channel: path(target.name, DRIVE), body: { command: 'drive', args } });
  const desc = `throttle ${args.throttle} steer ${args.steer}`;
  if (desc !== lastSent) {
    lastSent = desc;
    $('drive').textContent = desc;
  }
}

// 20 Hz while a key is held or "stream" is on. Stopping the stream lets the
// robot trip its dead-man after the lock's message_stale_ms, by design.
// (The relay drops drive messages from sessions that do not hold the lock.)
setInterval(() => {
  if (preset !== 'operator') return;
  if (keys.size > 0 || streaming) sendDrive();
}, 50);

function lockMsg(type: 'claim' | 'release', force = false) {
  const target = driveTarget();
  if (!target) return log('no broadcast with a drive lock');
  const lock = path(target.name, DRIVE);
  send({ type: 'control', msg: type === 'claim' ? { type, lock, force } : { type, lock } });
}

// E-stop: every robot in the room safe-stops, whoever drives it, and stays
// stopped until its drive lock is claimed again. This console stops
// sending and lets go of its own lock too.
function estop() {
  keys.clear();
  streaming = false;
  $<HTMLInputElement>('stream').checked = false;
  const targets = state?.broadcasts.filter((b) => b.channels.some((c) => c.info.id === ESTOP)) ?? [];
  if (!targets.length) log('E-STOP: no broadcast with an estop channel (not connected?)');
  for (const b of targets) {
    send({ type: 'estop', channel: path(b.name, ESTOP), body: { reason: 'console' } });
  }
  const mine = driveTarget()?.locks.find((l) => l.info.id === DRIVE)?.holder === session;
  if (mine && session) lockMsg('release');
}

function setupOperator() {
  $('operator').hidden = false;
  // Claiming while holding claims anew: that re-arms a robot after an
  // e-stop.
  $('claim').onclick = () => {
    if (driveTarget()?.locks.find((l) => l.info.id === DRIVE)?.holder === session && session) lockMsg('release');
    lockMsg('claim');
  };
  $('force').onclick = () => lockMsg('claim', true);
  $('release').onclick = () => lockMsg('release');
  $('once').onclick = sendDrive;
  $('estop').onclick = estop;
  const stream = $<HTMLInputElement>('stream');
  stream.onchange = () => (streaming = stream.checked);
  window.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    if (['w', 'a', 's', 'd'].includes(k)) {
      keys.add(k);
      e.preventDefault();
    }
    if (k === ' ') {
      estop();
      e.preventDefault();
    }
  });
  window.addEventListener('keyup', (e) => keys.delete(e.key.toLowerCase()));
  window.addEventListener('blur', () => keys.clear());
}

// --- worker messages -------------------------------------------------------

worker.onmessage = (e: MessageEvent<FromWorker>) => {
  const msg = e.data;
  switch (msg.type) {
    case 'status':
      $('conn').textContent = msg.state + (msg.detail ? `: ${msg.detail}` : '');
      $('conn').className = `pill ${msg.state === 'connected' ? 'ok' : msg.state === 'closed' ? 'bad' : 'warn'}`;
      if (msg.state !== 'connected') {
        session = null;
        state = null;
        statuses.clear();
        renderState();
      }
      break;
    case 'control':
      onControl(msg.msg);
      break;
    case 'room':
      onRoom(msg.msg);
      break;
    case 'data':
      onData(msg.track, msg.value, msg.atUs);
      break;
    case 'estop':
      log(
        msg.acked
          ? `E-STOP ${msg.channel}: robot ${msg.status === 'ok' ? 'stopped' : `answered ${msg.status} ${msg.detail ?? ''}`}`
          : `E-STOP ${msg.channel}: NO ANSWER from the robot in 2 s`,
      );
      break;
    case 'stats': {
      for (const [p, s] of Object.entries(msg.cameras)) renderCameraStats(p, s);
      $('clock').textContent = `relay rtt ${fmt(msg.clock.relayRttMs, 1)} ms · clock ${fmt(msg.clock.offsetMs, 1)} ms ±${fmt(msg.clock.errorMs, 1)} · ${fmt(1000 / msg.clock.refreshMs, 0)} Hz`;
      const t = msg.teleop;
      if (preset === 'operator') {
        const target = driveTarget();
        const status = target ? statuses.get(target.name)?.teleop : undefined;
        $('teleop-stats').textContent = [
          `ack rtt ${win(t.ackRtt)} ms   heartbeat rtt ${win(t.heartbeatRtt)} ms`,
          `acks ${Object.entries(t.acks)
            .map(([k, v]) => `${k}=${v}`)
            .join(' ') || '–'}${t.lastAckDetail ? `   last: ${t.lastAckDetail}` : ''}`,
          `robot (via heartbeat echo): ${t.tripped === null ? '–' : t.tripped ? 'tripped' : 'armed'}`,
          status
            ? `robot (via .status): commands ${status.commands}  last seq ${status.last_seq}  e-stops ${status.estops}`
            : 'robot (via .status): –',
        ].join('\n');
      }
      break;
    }
  }
};

// --- start -----------------------------------------------------------------

async function start() {
  $('title').textContent = room;
  $('role').textContent = preset;
  if (preset === 'operator') setupOperator();
  renderVersions();
  // The relay's HTTP base: ?relay_http=, else the one baked in at build
  // time (TQ_RELAY_HTTP), else this page's origin (the relay serves it).
  const relayHttp = new URL(params.get('relay_http') || TQ_RELAY_HTTP || location.origin);
  const info = (await (await fetch(new URL('/api/info', relayHttp))).json()) as {
    quic_port: number;
    sha256: string;
    self_signed: boolean;
    build?: BuildInfo;
  };
  if (compat(info.build) === 'incompatible') throw new Error(`incompatible relay: ${mismatch('relay', info.build)}`);
  let token = params.get('token');
  if (!token) {
    const r = await fetch(new URL(`/api/dev/token?preset=${preset}&room=${encodeURIComponent(room)}`, relayHttp));
    if (!r.ok) throw new Error(`no ?token= and the relay does not mint dev tokens (${r.status})`);
    token = ((await r.json()) as { token: string }).token;
  }
  // URL.hostname keeps the brackets around an IPv6 address.
  const host = params.get('relay') ?? `${relayHttp.hostname}:${info.quic_port}`;
  const url = `https://${host}/v1/room/${encodeURIComponent(room)}?token=${encodeURIComponent(token)}`;
  if (!('WebTransport' in window)) throw new Error('this browser has no WebTransport');
  if (!('VideoDecoder' in window)) throw new Error('this browser has no WebCodecs VideoDecoder');
  send({ type: 'connect', url, certHash: info.self_signed ? info.sha256 : null, sensorMs });
}

start().catch((e) => {
  $('conn').textContent = String(e);
  $('conn').className = 'pill bad';
});
