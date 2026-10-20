// Operator / viewer page. The media worker (worker.ts) does all transport,
// decoding and drawing; this thread only builds the UI, forwards operator
// input and shows the numbers.

import { type BuildInfo, type CameraInfo, type Control, type RelayState, type Role, compat, mismatch, shortBuild } from './protocol';
import { WEB_BUILD } from './version';
import type { CameraStats, FromWorker, ToWorker, Window } from './worker';

/** Set by build.sh from TQ_RELAY_HTTP; empty when the relay serves the page. */
declare const TQ_RELAY_HTTP: string;

const params = new URLSearchParams(location.search);
const av = params.get('av') ?? 'robot-01';
const role: Role = params.get('role') === 'operator' ? 'operator' : 'viewer';
// Photons -> capture timestamp (exposure, readout, ISP/USB) that the
// camera's timestamp does not cover; calibrate with an optical measurement.
const sensorMs = Number(params.get('sensor_ms') ?? 0) || 0;

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
let state: RelayState | null = null;
let relayBuild: BuildInfo | null = null;
let publisherSeen: string | null = null;
const tiles = new Map<string, { root: HTMLElement; overlay: HTMLElement; label: HTMLElement }>();

const fmt = (v: number | null | undefined, digits = 0) => (v === null || v === undefined ? '–' : v.toFixed(digits));
const win = (w: Window) => (w.avg === null ? '–' : `${w.avg.toFixed(1)}/${w.max!.toFixed(1)}`);
let syncErrorMs: number | null = null;

// --- tiles -----------------------------------------------------------------

function ensureTiles(cameras: CameraInfo[]) {
  const grid = $('grid');
  for (const cam of cameras) {
    if (tiles.has(cam.id)) {
      tiles.get(cam.id)!.label.textContent = `${cam.label} · ${cam.id}`;
      continue;
    }
    const root = el('div', 'tile');
    const canvas = el('canvas');
    const label = el('div', 'label', `${cam.label} · ${cam.id}`);
    const overlay = el('pre', 'overlay', 'waiting for keyframe…');
    const key = el('button', 'small', 'keyframe');
    key.title = 'Ask the publisher for a keyframe (the encoder has no hook; it logs the request)';
    key.onclick = () => send({ type: 'control', msg: { type: 'keyframe_request', camera: cam.id, reason: 'user' } });
    const bar = el('div', 'tilebar');
    bar.append(label, key);
    root.append(canvas, bar, overlay);
    grid.append(root);
    const offscreen = canvas.transferControlToOffscreen();
    send({ type: 'canvas', camera: cam.id, canvas: offscreen }, [offscreen]);
    tiles.set(cam.id, { root, overlay, label });
  }
}

function renderCameraStats(id: string, s: CameraStats) {
  const tile = tiles.get(id);
  if (!tile) return;
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
    `  glass→glass≈  ${win(s.glassToGlass)}  ±${fmt(syncErrorMs, 1)} sync${sensorMs ? ` +${sensorMs} sensor` : ''}`,
  ].join('\n');
  tile.root.classList.toggle('stale', s.fps < 0.5);
}

// --- versions ----------------------------------------------------------------

const describe = (b: BuildInfo | null | undefined) =>
  b ? `${shortBuild(b)}, protocol ${b.protocol.major}.${b.protocol.minor}` : 'unknown (protocol 1.0)';

// "web 0.1.0 · relay 0.1.0 · publisher 0.1.0"; amber when a protocol minor
// differs (works, but one side may lack features). Hover for commits.
function renderVersions() {
  const p = state?.publisher;
  const parts = [`web ${WEB_BUILD.version}`];
  const title = [`web ${describe(WEB_BUILD)}`];
  let warn = false;
  if (session) {
    parts.push(`relay ${relayBuild?.version ?? '?'}`);
    title.push(`relay ${describe(relayBuild)}`);
    warn ||= compat(relayBuild) !== 'same';
  }
  if (p) {
    parts.push(`publisher ${p.build?.version ?? '?'}`);
    title.push(`publisher ${describe(p.build)}`);
    warn ||= compat(p.build) !== 'same';
  }
  const pill = $('versions');
  pill.textContent = parts.join(' · ');
  pill.title = title.join('\n');
  pill.className = `pill ${warn ? 'warn' : ''}`;

  if (p) {
    const line = `publisher ${p.publisher_id} runs ${describe(p.build)}`;
    if (line !== publisherSeen) {
      publisherSeen = line;
      log(line);
      if (compat(p.build) !== 'same') log(`warning: ${mismatch('publisher', p.build)}`);
    }
  }
}

// --- relay / publisher state ----------------------------------------------

function renderState() {
  if (!state) return;
  const p = state.publisher;
  if (p) ensureTiles(p.cameras);
  const holder = state.operator;
  const mine = holder !== null && holder === session;
  $('claim-status').textContent = holder === null ? 'free' : mine ? 'you are driving' : `held by ${holder.slice(0, 8)}`;
  $('claim-status').className = `pill ${mine ? 'ok' : holder ? 'warn' : ''}`;
  document.body.classList.toggle('driving', mine);

  const pub = $('publisher');
  if (!p) {
    pub.textContent = 'publisher offline';
    pub.className = 'pill bad';
  } else {
    pub.textContent = `${p.publisher_id} · rtt ${fmt(p.rtt_ms, 1)} ms · clock ${fmt(
      p.clock_offset_us === null ? null : p.clock_offset_us / 1000,
      1,
    )} ms`;
    pub.className = 'pill ok';
  }
  const t = p?.teleop;
  const dm = $('deadman');
  if (!t) {
    dm.textContent = 'dead-man: –';
    dm.className = 'deadman';
  } else {
    dm.textContent = t.deadman_tripped ? `SAFE-STOPPED: ${t.reason}` : 'ARMED';
    dm.className = `deadman ${t.deadman_tripped ? 'tripped' : 'armed'}`;
  }
  $('viewers').textContent = `${state.operators} operator(s), ${state.viewers} viewer(s)`;
  renderVersions();

  const rows: string[] = [];
  if (p?.quic) {
    rows.push(
      `uplink quic: rtt ${p.quic.rtt_ms.toFixed(1)} ms  cwnd ${(p.quic.cwnd / 1024).toFixed(0)} KiB  lost ${p.quic.lost_packets}/${p.quic.sent_packets}  cong ${p.quic.congestion_events}`,
    );
  }
  for (const c of p?.camera_status ?? []) {
    rows.push(
      `${c.id}: ${c.online ? 'online' : 'OFFLINE'} ${c.codec_string ?? ''} ${c.fps.toFixed(1)} fps ${c.kbps.toFixed(0)} kbps  uplink drops ${c.dropped}  capture→pub ${fmt(c.capture_to_publisher_ms, 1)} ms`,
    );
  }
  $('publisher-detail').textContent = rows.join('\n');
}

function onControl(msg: Control) {
  switch (msg.type) {
    case 'welcome':
      session = msg.session;
      state = msg.state;
      relayBuild = msg.relay ?? null;
      log(`relay runs ${describe(relayBuild)}`);
      if (compat(relayBuild) !== 'same') log(`warning: ${mismatch('relay', relayBuild)}`);
      $('session').textContent = `${msg.role} ${msg.session.slice(0, 8)}`;
      renderState();
      break;
    case 'state':
      state = msg.state;
      renderState();
      break;
    case 'claim_result':
      if (!msg.ok && msg.reason) log(`claim refused: ${msg.reason}`);
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

function log(line: string) {
  const box = $('log');
  box.textContent = `${new Date().toLocaleTimeString()} ${line}\n${box.textContent}`.slice(0, 4000);
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

// 20 Hz while a key is held or "stream" is on. Stopping the stream lets the
// robot trip its dead-man after command_stale_ms, by design.
setInterval(() => {
  if (role !== 'operator') return;
  const active = keys.size > 0 || streaming;
  if (!active) return;
  const args = driveCommand();
  send({ type: 'teleop', command: 'drive', args });
  const desc = `throttle ${args.throttle} steer ${args.steer}`;
  if (desc !== lastSent) {
    lastSent = desc;
    $('drive').textContent = desc;
  }
}, 50);

function setupOperator() {
  $('operator').hidden = false;
  $('claim').onclick = () => send({ type: 'control', msg: { type: 'claim', force: false } });
  $('force').onclick = () => send({ type: 'control', msg: { type: 'claim', force: true } });
  $('release').onclick = () => send({ type: 'control', msg: { type: 'release' } });
  $('once').onclick = () => send({ type: 'teleop', command: 'ping', args: {} });
  const stream = $<HTMLInputElement>('stream');
  stream.onchange = () => (streaming = stream.checked);
  window.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    if (['w', 'a', 's', 'd'].includes(k)) {
      keys.add(k);
      e.preventDefault();
    }
    if (k === ' ') {
      // Emergency stop: drop the claim; the robot trips immediately.
      keys.clear();
      streaming = false;
      stream.checked = false;
      send({ type: 'control', msg: { type: 'release' } });
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
      if (msg.state !== 'connected') session = null;
      break;
    case 'control':
      onControl(msg.msg);
      break;
    case 'stats': {
      for (const [id, s] of Object.entries(msg.cameras)) renderCameraStats(id, s);
      syncErrorMs = msg.clock.syncErrorMs;
      $('clock').textContent = `relay rtt ${fmt(msg.clock.relayRttMs, 1)} ms · clock ${fmt(msg.clock.offsetMs, 1)} ms · sync ±${fmt(msg.clock.syncErrorMs, 1)} ms · ${fmt(1000 / msg.clock.refreshMs, 0)} Hz`;
      const t = msg.teleop;
      if (role === 'operator') {
        $('teleop-stats').textContent = [
          `ack rtt ${win(t.ackRtt)} ms   heartbeat rtt ${win(t.heartbeatRtt)} ms`,
          `acks ${Object.entries(t.acks)
            .map(([k, v]) => `${k}=${v}`)
            .join(' ') || '–'}${t.lastAckDetail ? `   last: ${t.lastAckDetail}` : ''}`,
          `robot (via heartbeat echo): ${t.tripped === null ? '–' : t.tripped ? 'tripped' : 'armed'}`,
        ].join('\n');
      }
      break;
    }
  }
};

// --- start -----------------------------------------------------------------

async function start() {
  $('title').textContent = `${av}`;
  $('role').textContent = role;
  if (role === 'operator') setupOperator();
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
    const r = await fetch(new URL(`/api/dev/token?role=${role}&av=${encodeURIComponent(av)}`, relayHttp));
    if (!r.ok) throw new Error(`no ?token= and the relay does not mint dev tokens (${r.status})`);
    token = ((await r.json()) as { token: string }).token;
  }
  // URL.hostname keeps the brackets around an IPv6 address.
  const host = params.get('relay') ?? `${relayHttp.hostname}:${info.quic_port}`;
  const url = `https://${host}/v1/session/${encodeURIComponent(av)}?token=${encodeURIComponent(token)}`;
  if (!('WebTransport' in window)) throw new Error('this browser has no WebTransport');
  if (!('VideoDecoder' in window)) throw new Error('this browser has no WebCodecs VideoDecoder');
  send({ type: 'connect', url, certHash: info.self_signed ? info.sha256 : null, role, sensorMs, hello: !!info.build });
}

start().catch((e) => {
  $('conn').textContent = String(e);
  $('conn').className = 'pill bad';
});
