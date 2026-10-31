// Operator / viewer page. The media worker (worker.ts) does all transport,
// decoding and drawing of video; this thread runs the board (a layout of
// widgets, board.ts), the remote stop, the room log and the header.

import { Board } from './board';
import { Catalog } from './catalog';
import {
  type BuildInfo,
  type Control,
  type ParticipantInfo,
  type Path,
  type RobotStatus,
  type RoomEventBody,
  type RoomMessage,
  type RoomState,
  compat,
  mismatch,
  path,
  shortBuild,
} from './protocol';
import { WEB_BUILD } from './version';
import { type App, type TeleopStats, fmt } from './widgets';
import type { DataItem, FromWorker, ToWorker } from './worker';

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
const STOP = 'stop';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const worker = new Worker('/worker.js', { type: 'module' });
const send = (msg: ToWorker, transfer: Transferable[] = []) => worker.postMessage(msg, transfer);

let session: string | null = null;
let state: RoomState | null = null;
let relayBuild: BuildInfo | null = null;
let teleop: TeleopStats | null = null;
const ownersSeen = new Map<string, string>();
/** Each broadcast's latest `.status` message. */
const statuses = new Map<string, RobotStatus>();
const catalog = new Catalog();
const keys = new Set<string>();
const logLines: { atMs: number; text: string }[] = [];

const lockHolder = (lock: Path) => {
  const [b, id] = lock.split('/');
  return state?.broadcasts.find((x) => x.name === b)?.locks.find((l) => l.info.id === id)?.holder ?? null;
};

function lockMsg(lock: Path, type: 'claim' | 'release', force = false) {
  send({ type: 'control', msg: type === 'claim' ? { type, lock, force } : { type, lock } });
}

const DEADZONE = 0.15;

const app: App = {
  send,
  catalog,
  get session() {
    return session;
  },
  get state() {
    return state;
  },
  operator: preset === 'operator',
  who,
  statuses,
  get teleop() {
    return teleop;
  },
  get logLines() {
    return logLines.map((l) => l.text);
  },
  lock(lock, action) {
    if (action === 'release') return lockMsg(lock, 'release');
    if (action === 'force') return lockMsg(lock, 'claim', true);
    // Claiming while holding claims anew: that re-arms a robot after a
    // remote stop.
    if (session && lockHolder(lock) === session) lockMsg(lock, 'release');
    lockMsg(lock, 'claim');
  },
  driveInput(source) {
    if (source !== 'gamepad' && keys.size > 0) {
      return {
        throttle: (keys.has('w') ? 1 : 0) - (keys.has('s') ? 1 : 0),
        steer: (keys.has('d') ? 1 : 0) - (keys.has('a') ? 1 : 0),
      };
    }
    if (source !== 'keyboard') {
      const pad = navigator.getGamepads?.().find((g) => g?.connected);
      if (pad) {
        const dz = (v: number | undefined) => (v === undefined || Math.abs(v) < DEADZONE ? 0 : v);
        const steer = dz(pad.axes[0]);
        const throttle = -dz(pad.axes[1]);
        if (steer || throttle) return { throttle, steer };
      }
    }
    return null;
  },
};

const board = new Board($('board'), $('toolbar'), app, `tq.layout.${room}.${preset}`);

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

// Each robot's dead-man, compactly: whatever the layout shows, the header
// says when a robot is safe-stopped.
function renderRobots() {
  const parts: string[] = [];
  let tripped = false;
  for (const b of state?.broadcasts ?? []) {
    const t = b.owner ? statuses.get(b.name)?.teleop : undefined;
    if (!t) continue;
    parts.push(`${b.name} ${t.tripped ? 'SAFE-STOPPED' : 'armed'}`);
    tripped ||= t.tripped;
  }
  const pill = $('robots');
  pill.textContent = parts.join(' · ');
  pill.hidden = !parts.length;
  pill.className = `pill ${tripped ? 'bad' : 'ok'}`;
}

function renderState() {
  catalog.state = state;
  if (!state) {
    // Disconnected: nothing room-dependent is known until the next state.
    document.body.classList.remove('driving');
    $('publisher').textContent = 'no broadcasts';
    $('publisher').className = 'pill bad';
    $('viewers').textContent = '';
    renderRobots();
    renderVersions();
    board.update();
    return;
  }
  for (const name of statuses.keys()) if (!state.broadcasts.some((b) => b.name === name)) statuses.delete(name);
  document.body.classList.toggle('driving', !!session && state.broadcasts.some((b) => b.locks.some((l) => l.holder === session)));

  const pub = $('publisher');
  if (!state.broadcasts.length) {
    pub.textContent = 'no broadcasts';
    pub.className = 'pill bad';
  } else {
    pub.textContent = state.broadcasts.map((b) => (b.owner ? `${b.name} · rtt ${fmt(b.rtt_ms, 1)} ms` : `${b.name} offline`)).join('  |  ');
    const offline = state.broadcasts.filter((b) => !b.owner).length;
    pub.className = `pill ${offline === 0 ? 'ok' : offline === state.broadcasts.length ? 'bad' : 'warn'}`;
  }
  const count = (k: ReturnType<typeof kind>) => state!.participants.filter((x) => kind(x) === k).length;
  $('viewers').textContent = `${count('operator')} operator(s), ${count('viewer')} viewer(s), ${count('publisher')} publisher(s)`;
  renderRobots();
  renderVersions();
  board.update();

  const rows: string[] = [];
  for (const b of state.broadcasts) {
    if (b.quic) {
      rows.push(
        `${b.name} uplink quic: rtt ${b.quic.rtt_ms.toFixed(1)} ms  cwnd ${(b.quic.cwnd / 1024).toFixed(0)} KiB  lost ${b.quic.lost_packets}/${b.quic.sent_packets}  cong ${b.quic.congestion_events}  clock ${fmt(b.clock_offset_us === null ? null : b.clock_offset_us / 1000, 1)} ms`,
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
      catalog.perms = msg.perms;
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

// Data track messages: every one feeds the catalog's fields; `.status`
// also feeds the header, `.events` the log. Then the widgets get them.
function onData(items: DataItem[]) {
  const now = performance.now();
  let robots = false;
  for (const { track, value, atUs } of items) {
    catalog.observe(track, value, now);
    const [broadcast, name] = track.split('/') as [string, string];
    if (name === '.status') {
      const prev = statuses.get(broadcast)?.teleop;
      const next = value as RobotStatus;
      statuses.set(broadcast, next);
      if (prev && next.teleop && prev.tripped !== next.teleop.tripped) {
        log(next.teleop.tripped ? `${broadcast} safe-stopped: ${next.teleop.reason}` : `${broadcast} armed`);
      }
      robots = true;
    } else if (name === '.events') {
      log(`${broadcast}: ${JSON.stringify(value)}`, atUs / 1000);
    }
  }
  if (robots) renderRobots();
  board.data(items);
}

// Newest first. A line with a time (Unix ms) goes after any newer ones: a
// broadcast's `.events` replayed on subscribing can be older than lines
// before it.
function log(line: string, atMs = Date.now()) {
  const i = logLines.findIndex((l) => l.atMs <= atMs);
  logLines.splice(i < 0 ? logLines.length : i, 0, { atMs, text: `${new Date(atMs).toLocaleTimeString()} ${line}` });
  logLines.length = Math.min(logLines.length, 200);
  board.logChanged();
}

// --- remote stop and drive keys ----------------------------------------------

// Remote stop: every robot in the room safe-stops, whoever drives it, and
// stays stopped until its drive lock is claimed again. This console stops
// sending and lets go of its own drive locks too. It is not part of any
// layout, so no layout can remove it.
function remoteStop() {
  keys.clear();
  board.stopped();
  const targets = state?.broadcasts.filter((b) => b.channels.some((c) => c.info.id === STOP)) ?? [];
  if (!targets.length) log('STOP: no broadcast with a stop channel (not connected?)');
  for (const b of targets) {
    send({ type: 'stop', channel: path(b.name, STOP), body: { reason: 'console' } });
  }
  $('stop-status').textContent = targets.length ? `STOP sent to ${targets.map((b) => b.name).join(', ')}` : 'STOP: no robot';
  $('stop-status').className = `pill ${targets.length ? 'warn' : 'bad'}`;
  if (!session) return;
  const drives = new Set([...board.driveLocks(), ...(state?.broadcasts ?? []).map((b) => path(b.name, DRIVE))]);
  for (const lock of drives) if (lockHolder(lock) === session) lockMsg(lock, 'release');
}

// Fields that take typed text: WASD typed there does not drive.
const typing = (t: EventTarget | null) =>
  t instanceof HTMLTextAreaElement ||
  t instanceof HTMLSelectElement ||
  (t instanceof HTMLInputElement && !['checkbox', 'radio', 'range', 'button', 'submit'].includes(t.type));

function setupOperator() {
  $('stop').hidden = false;
  $('stop').onclick = remoteStop;
  window.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    if (['w', 'a', 's', 'd'].includes(k) && !typing(e.target) && !e.metaKey && !e.ctrlKey) {
      keys.add(k);
      e.preventDefault();
    }
  });
  window.addEventListener('keyup', (e) => keys.delete(e.key.toLowerCase()));
  // Space is STOP, everywhere, before anything else sees it (capture
  // phase): a focused button or checkbox would otherwise take it as a click
  // too (a focused Claim would re-claim, a stream box would stream again).
  window.addEventListener(
    'keydown',
    (e) => {
      if (e.key !== ' ') return;
      e.preventDefault();
      e.stopPropagation();
      if (!e.repeat) remoteStop();
    },
    true,
  );
  window.addEventListener(
    'keyup',
    (e) => {
      if (e.key !== ' ') return;
      e.preventDefault();
      e.stopPropagation();
    },
    true,
  );
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
    case 'batch':
      if (msg.data.length) onData(msg.data);
      if (msg.acks.length) board.acks(msg.acks);
      break;
    case 'stop': {
      const line = msg.acked
        ? `STOP ${msg.channel}: robot ${msg.status === 'ok' ? 'stopped' : `answered ${msg.status} ${msg.detail ?? ''}`}`
        : `STOP ${msg.channel}: NO ANSWER from the robot in 2 s`;
      log(line);
      $('stop-status').textContent = line;
      $('stop-status').className = `pill ${msg.acked && msg.status === 'ok' ? 'ok' : 'bad'}`;
      break;
    }
    case 'stats': {
      board.cameras(msg.cameras);
      teleop = msg.teleop;
      $('clock').textContent = `relay rtt ${fmt(msg.clock.relayRttMs, 1)} ms · clock ${fmt(msg.clock.offsetMs, 1)} ms ±${fmt(msg.clock.errorMs, 1)} · ${fmt(1000 / msg.clock.refreshMs, 0)} Hz`;
      const ui = board.takeUiStats();
      $('ui').textContent = `ui ${ui.avg.toFixed(2)} ms/frame · max ${ui.max.toFixed(1)}`;
      $('ui').className = `pill ${ui.max > 8 ? 'warn' : ''}`;
      $('ui').title = 'Main-thread time drawing widgets per display frame. Video is drawn in the media worker and does not wait for it.';
      break;
    }
  }
};

// --- start -----------------------------------------------------------------

async function start() {
  $('title').textContent = room;
  $('role').textContent = preset;
  if (preset === 'operator') setupOperator();
  const edit = $('edit');
  edit.onclick = () => board.setEditing(!board.editing);
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
