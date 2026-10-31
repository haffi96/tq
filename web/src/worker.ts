// Media worker: owns the WebTransport session, reads the room's events
// stream, subscribes to the tracks the page's widgets want (and every
// broadcast's `.status` and `.events` data tracks), decodes video with
// WebCodecs and draws each frame on its widgets' OffscreenCanvases the
// moment it is decoded. No jitter buffer: a frame is shown as soon as it is
// complete, and nothing the page does can hold it up. Channel messages and
// lock heartbeats also go out from here so they never wait behind the UI
// thread. Data and acks go to the page in batches, at most once per
// display frame: ingest runs at the data's rate, the page draws at its own.

import { SpsPatcher } from './h264sps';
import {
  type BroadcastState,
  type Control,
  type Delivery,
  type FrameHeader,
  type GroupHeader,
  type Path,
  type RoomMessage,
  type RoomState,
  type Subscription,
  type TrackInfo,
  EVENTS_VERSION,
  FLAG_CAPTURE_FROM_SEI,
  FLAG_KEY,
  FRAME_HEADER_LEN,
  GROUP_HEADER_FIXED,
  MAX_FRAME_LEN,
  MAX_ROOM_MESSAGE_LEN,
  STREAM_TYPE_EVENTS,
  STREAM_TYPE_GROUP,
  StreamReader,
  decodeControl,
  decodeDatagram,
  decodeFrameHeader,
  decodeGroupFixed,
  decodeJson,
  decodeRoomMessage,
  encodeControl,
  encodeHeartbeat,
  encodeJsonMessage,
  nowUs,
  path,
} from './protocol';
import { WEB_BUILD } from './version';

export type ToWorker =
  | { type: 'connect'; url: string; certHash: string | null; sensorMs: number }
  /** A video widget's canvas: frames of `track` are drawn on it. */
  | { type: 'canvas'; id: string; track: Path; canvas: OffscreenCanvas }
  /** The widget is gone: stop drawing on its canvas. */
  | { type: 'uncanvas'; id: string }
  /** Everything the widgets are bound to; replaces the previous list. */
  | { type: 'want'; subs: Subscription[] }
  | { type: 'control'; msg: Control }
  /** One JSON message on a channel (`<broadcast>/<channel>`). */
  | { type: 'message'; channel: Path; body: unknown }
  /** A remote stop: sent again every STOP_RETRY_MS until acked. */
  | { type: 'stop'; channel: Path; body: unknown };

export interface Window {
  avg: number | null;
  max: number | null;
}

export interface CameraStats {
  codec: string | null;
  size: string;
  fps: number;
  kbps: number;
  frames: number;
  decoded: number;
  dropped: number;
  keyframes: number;
  groups: number;
  errors: number;
  decodeQueue: number;
  spsPatched: boolean;
  captureFromSei: boolean;
  // Error bound of the owner -> browser clock mapping (both hops), ms.
  syncErrorMs: number | null;
  // Per-hop latency over the last report window (ms).
  captureToPublisher: Window;
  publisherToRelay: Window;
  relayToBrowser: Window;
  decode: Window;
  captureToDrawn: Window;
  // drawImage -> estimated photons on the operator's screen.
  drawToDisplay: Window;
  // Estimated glass to glass: sensor offset + capture -> displayed.
  glassToGlass: Window;
}

export type FromWorker =
  | { type: 'status'; state: 'connecting' | 'connected' | 'closed'; detail?: string }
  | { type: 'control'; msg: Control }
  | { type: 'room'; msg: RoomMessage }
  /** How a remote stop went: acked by the robot, or not within STOP_GIVE_UP_MS. */
  | { type: 'stop'; channel: Path; acked: boolean; status?: string; detail?: string }
  /** Messages of subscribed JSON data tracks, and acks of channel
   *  messages, since the last batch. */
  | { type: 'batch'; data: DataItem[]; acks: AckItem[] }
  | {
      type: 'stats';
      cameras: Record<Path, CameraStats>;
      clock: {
        relayRttMs: number | null;
        offsetMs: number | null;
        // Error bound of the relay -> browser clock mapping.
        errorMs: number | null;
        refreshMs: number;
        sensorMs: number;
      };
      teleop: {
        ackRtt: Window;
        heartbeatRtt: Window;
        acks: Record<string, number>;
        lastAckDetail: string | null;
        tripped: boolean | null;
      };
    };

/** A data track message and its capture time on this machine's clock
 *  (Unix µs; arrival time when unknown). */
export interface DataItem {
  track: Path;
  value: unknown;
  atUs: number;
}

export interface AckItem {
  channel: Path;
  seq: number;
  status: string;
  detail: string;
  rttMs: number;
}

const scope = self as unknown as {
  postMessage(msg: FromWorker): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
};
const post = (msg: FromWorker) => scope.postMessage(msg);

const DECODE_STALL_MS = 150;
const PENDING_EXPIRY_MS = 1000;
const KEYFRAME_REQUEST_MS = 500;
const HEARTBEAT_MS = 100;
/** How long a group stream waits for the state that names its track. */
const STATE_WAIT_MS = 2000;
/** Data tracks every console subscribes to (crates/tq-teleop). */
const WATCHED_DATA = ['.status', '.events'];
/** Batches go out at most this often. */
const BATCH_MS = 8;

class Win {
  private sum = 0;
  private n = 0;
  private max = -Infinity;
  add(v: number) {
    this.sum += v;
    this.n += 1;
    if (v > this.max) this.max = v;
  }
  take(): Window {
    const out = this.n ? { avg: this.sum / this.n, max: this.max } : { avg: null, max: null };
    this.sum = 0;
    this.n = 0;
    this.max = -Infinity;
    return out;
  }
}

interface Pending {
  captureUs: number;
  recvUs: number;
  submitMs: number;
  sei: boolean;
}

/** A subscribed track that group streams (and datagrams) feed. */
interface Track {
  readonly path: Path;
  /** A new group stream; false if it is not wanted (older than ours). */
  startGroup(header: GroupHeader, reader: StreamReader): boolean;
  /** Whether frames of `group` are still wanted. */
  current(group: bigint): boolean;
  frame(header: FrameHeader, data: Uint8Array, codec: string): void;
  reset(): void;
}

class Camera implements Track {
  /** Widget id -> its canvas. */
  canvases = new Map<string, { canvas: OffscreenCanvas; ctx: OffscreenCanvasRenderingContext2D | null }>();
  size = '-';
  decoder: VideoDecoder | null = null;
  codec: string | null = null;
  group = -1n;
  reader: StreamReader | null = null;
  waitKey = true;
  sps = new SpsPatcher();
  pending = new Map<number, Pending>();
  nextTs = 0;
  lastKeyRequest = 0;
  c = { frames: 0, decoded: 0, dropped: 0, keyframes: 0, groups: 0, errors: 0, bytes: 0 };
  last = { decoded: 0, bytes: 0, at: performance.now() };
  captureFromSei = false;
  w = {
    captureToPublisher: new Win(),
    publisherToRelay: new Win(),
    relayToBrowser: new Win(),
    decode: new Win(),
    captureToDrawn: new Win(),
    drawToDisplay: new Win(),
    glassToGlass: new Win(),
  };

  constructor(
    readonly path: Path,
    readonly broadcast: string,
  ) {}

  addCanvas(id: string, canvas: OffscreenCanvas) {
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true }) as OffscreenCanvasRenderingContext2D | null;
    this.canvases.set(id, { canvas, ctx });
  }

  // A new group (GOP) stream arrived. Older groups are cancelled: the new
  // one starts with a keyframe, so nothing in them is needed any more.
  startGroup(header: GroupHeader, reader: StreamReader): boolean {
    if (header.group <= this.group) return false;
    this.reader?.cancel();
    this.reader = reader;
    this.group = header.group;
    this.c.groups += 1;
    return true;
  }

  current(group: bigint): boolean {
    return this.group === group;
  }

  reset() {
    this.reader?.cancel();
    this.reader = null;
    this.group = -1n;
    this.waitKey = true;
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.decoder = null;
    this.codec = null;
    this.pending.clear();
  }

  configure(codec: string) {
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.pending.clear();
    this.codec = codec;
    this.decoder = new VideoDecoder({
      output: (frame) => this.draw(frame),
      error: (e) => this.failed(e),
    });
    const hevc = codec.startsWith('hev1') || codec.startsWith('hvc1');
    // No description: chunks are Annex B with in-band parameter sets.
    // Chrome only decodes HEVC in hardware.
    this.decoder.configure({
      codec,
      optimizeForLatency: true,
      hardwareAcceleration: hevc ? 'prefer-hardware' : 'no-preference',
    });
    this.waitKey = true;
  }

  failed(e: unknown) {
    this.c.errors += 1;
    console.error(`[${this.path}] decoder error`, e);
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.waitKey = true;
    requestKeyframe(this, 'decoder error');
  }

  frame(header: FrameHeader, data: Uint8Array, codec: string) {
    const recvUs = nowUs();
    const key = (header.flags & FLAG_KEY) !== 0;
    const sei = (header.flags & FLAG_CAPTURE_FROM_SEI) !== 0;
    this.captureFromSei = sei;
    this.c.frames += 1;
    this.c.bytes += data.length;

    const pubOff = ownerClock(this.broadcast)?.offsetUs ?? null;
    const myOff = clock.myOffsetUs;
    this.w.captureToPublisher.add((header.publisherUs - header.captureUs) / 1000);
    if (pubOff !== null && myOff !== null) {
      this.w.publisherToRelay.add((header.relayUs - (header.publisherUs - pubOff)) / 1000);
      this.w.relayToBrowser.add((recvUs - myOff - header.relayUs) / 1000);
    }

    if (key) {
      this.c.keyframes += 1;
      if (codec.startsWith('avc1')) data = this.sps.apply(data);
      if (codec !== this.codec || !this.decoder || this.decoder.state === 'closed') this.configure(codec);
    }
    const d = this.decoder;
    if (!d || d.state !== 'configured' || (this.waitKey && !key)) {
      this.c.dropped += 1;
      return;
    }
    // A decoder that has held a frame for longer than DECODE_STALL_MS is
    // not keeping up: skip to the next keyframe rather than build latency.
    // (Queue depth alone would also fire on the join burst, when the
    // relay's cached GOP arrives at once and decodes in a few ms.)
    const now = performance.now();
    for (const [ts, p] of this.pending) {
      if (now - p.submitMs < PENDING_EXPIRY_MS) break;
      this.pending.delete(ts); // never produced output (decoder dropped it)
    }
    const oldest = this.pending.values().next().value;
    if (!key && d.decodeQueueSize > 0 && oldest && now - oldest.submitMs > DECODE_STALL_MS) {
      this.c.dropped += 1;
      this.waitKey = true;
      requestKeyframe(this, 'decoder backlog');
      return;
    }
    this.waitKey = false;
    const ts = this.nextTs++;
    this.pending.set(ts, { captureUs: header.captureUs, recvUs, submitMs: now, sei });
    try {
      d.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data }));
    } catch (e) {
      this.failed(e);
    }
  }

  draw(frame: VideoFrame) {
    const info = this.pending.get(frame.timestamp);
    this.pending.delete(frame.timestamp);
    for (const { canvas, ctx } of this.canvases.values()) {
      if (!ctx) continue;
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
    }
    this.size = `${frame.displayWidth}x${frame.displayHeight}`;
    frame.close();
    this.c.decoded += 1;
    if (!info) return;
    this.w.decode.add(performance.now() - info.submitMs);
    // Capture times are only real with the encoder's SEI; otherwise they
    // are the owner's receive time and capture -> glass would be a guess.
    if (!info.sei) return;
    const pubOff = ownerClock(this.broadcast)?.offsetUs ?? null;
    const myOff = clock.myOffsetUs;
    if (pubOff === null || myOff === null) return;
    // Capture time on the browser clock: owner clock -> relay -> here.
    const captureHere = info.captureUs - pubOff + myOff;
    const drawnUs = nowUs();
    this.w.captureToDrawn.add((drawnUs - captureHere) / 1000);
    // The canvas update goes out with the next rendered frame: the next
    // animation frame starts it and scan-out puts it on the panel about one
    // refresh later (the same notion as expectedDisplayTime in
    // requestVideoFrameCallback). Panel response time is not included.
    nextFrame((frameUs) => {
      const displayedUs = frameUs + display.intervalMs * 1000;
      this.w.drawToDisplay.add((displayedUs - drawnUs) / 1000);
      this.w.glassToGlass.add((displayedUs - captureHere) / 1000 + sensorMs);
    });
  }

  stats(): CameraStats {
    const now = performance.now();
    const secs = Math.max((now - this.last.at) / 1000, 1e-3);
    const fps = (this.c.decoded - this.last.decoded) / secs;
    const kbps = ((this.c.bytes - this.last.bytes) * 8) / 1000 / secs;
    this.last = { decoded: this.c.decoded, bytes: this.c.bytes, at: now };
    const owner = ownerClock(this.broadcast);
    return {
      codec: this.codec,
      size: this.size,
      fps,
      kbps,
      ...this.c,
      decodeQueue: this.decoder?.decodeQueueSize ?? 0,
      spsPatched: this.sps.patched,
      captureFromSei: this.captureFromSei,
      syncErrorMs: owner && clock.myErrorUs !== null ? (owner.errorUs + clock.myErrorUs) / 1000 : null,
      captureToPublisher: this.w.captureToPublisher.take(),
      publisherToRelay: this.w.publisherToRelay.take(),
      relayToBrowser: this.w.relayToBrowser.take(),
      decode: this.w.decode.take(),
      captureToDrawn: this.w.captureToDrawn.take(),
      drawToDisplay: this.w.drawToDisplay.take(),
      glassToGlass: this.w.glassToGlass.take(),
    };
  }
}

// A JSON data track: each message goes to the UI as is. `latest` groups
// supersede each other like GOPs; `stream` groups are all read, in order.
class DataTrack implements Track {
  group = -1n;
  reader: StreamReader | null = null;

  constructor(
    readonly path: Path,
    readonly delivery: Delivery,
  ) {}

  startGroup(header: GroupHeader, reader: StreamReader): boolean {
    if (this.delivery === 'stream') return true;
    if (header.group <= this.group) return false;
    this.reader?.cancel();
    this.reader = reader;
    this.group = header.group;
    return true;
  }

  current(group: bigint): boolean {
    return this.delivery === 'stream' || this.group === group;
  }

  frame(header: FrameHeader, data: Uint8Array, codec: string) {
    if (codec !== 'json') return;
    let value: unknown;
    try {
      value = decodeJson(data);
    } catch {
      return; // Not JSON after all; nothing to show.
    }
    // The relay replays the newest group on subscribing, so a message can
    // be old: say when it was captured, on this clock. Right after
    // connecting the offsets may not be measured yet: a `stream` message
    // (an event, logged in time order) waits up to a second for them;
    // others (latest wins, so order matters) use the arrival time.
    const recvUs = nowUs();
    const broadcast = this.path.split('/')[0]!;
    const deliver = (tries: number) => {
      const pubOff = ownerClock(broadcast)?.offsetUs ?? null;
      const myOff = clock.myOffsetUs;
      if ((pubOff === null || myOff === null) && tries > 0) return void setTimeout(() => deliver(tries - 1), 50);
      const atUs = pubOff === null || myOff === null ? recvUs : header.captureUs - pubOff + myOff;
      queueData({ track: this.path, value, atUs });
    };
    deliver(this.delivery === 'stream' ? 20 : 0);
  }

  reset() {
    this.reader?.cancel();
    this.reader = null;
    this.group = -1n;
  }
}

// Clock offsets (microseconds): this browser minus the relay, with its
// error bound (half the best round trip it came from). Each broadcast
// owner's offset to the relay comes with the room's state.
const clock = {
  myOffsetUs: null as number | null,
  myErrorUs: null as number | null,
  relayRttMs: null as number | null,
};

/** Owner clock minus relay clock for a broadcast, from the room's state. */
function ownerClock(broadcast: string): { offsetUs: number; errorUs: number } | null {
  const b = room?.broadcasts.find((x) => x.name === broadcast);
  if (!b || b.clock_offset_us == null) return null;
  return { offsetUs: b.clock_offset_us, errorUs: b.clock_error_us ?? 0 };
}

// Same filter as tq_wire::ClockFilter: of the last CLOCK_WINDOW pings,
// trust the one with the smallest round trip (least queueing, least
// asymmetry); half its round trip bounds the error.
const CLOCK_WINDOW = 20;
const PING_MS = 500;
const clockSamples: { offsetUs: number; rttUs: number }[] = [];

function addClockSample(offsetUs: number, rttUs: number) {
  clockSamples.push({ offsetUs, rttUs });
  if (clockSamples.length > CLOCK_WINDOW) clockSamples.shift();
  let best = clockSamples[0]!;
  for (const s of clockSamples) if (s.rttUs < best.rttUs) best = s;
  clock.myOffsetUs = best.offsetUs;
  clock.myErrorUs = best.rttUs / 2;
}

// Display refresh, measured from animation frames in this worker (Chrome
// runs requestAnimationFrame in workers that own an OffscreenCanvas).
// Without it, assume 60 Hz and the next frame half an interval away.
let sensorMs = 0;
const display = { intervalMs: 1000 / 60, last: null as number | null };
const workerRaf = (self as unknown as { requestAnimationFrame?: (cb: (ts: number) => void) => number })
  .requestAnimationFrame;

function nextFrame(cb: (frameUs: number) => void) {
  if (workerRaf) {
    workerRaf((ts) => cb(Math.round((performance.timeOrigin + ts) * 1000)));
  } else {
    cb(nowUs() + display.intervalMs * 500);
  }
}

if (workerRaf) {
  const tick = (ts: number) => {
    if (display.last !== null) {
      const dt = ts - display.last;
      if (dt > 2 && dt < 50) display.intervalMs = display.intervalMs * 0.95 + dt * 0.05;
    }
    display.last = ts;
    workerRaf(tick);
  };
  workerRaf(tick);
}

const teleop = {
  ackRtt: new Win(),
  heartbeatRtt: new Win(),
  acks: {} as Record<string, number>,
  lastAckDetail: null as string | null,
  tripped: null as boolean | null,
};

let transport: WebTransport | null = null;
let controlWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
let datagramWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
let connectArgs: { url: string; certHash: string | null } | null = null;
let session: string | null = null;
let room: RoomState | null = null;
const tracks = new Map<Path, Track>();
const cameras = new Map<Path, Camera>();
/** Widget id -> the track it draws and its canvas. */
const canvases = new Map<string, { track: Path; canvas: OffscreenCanvas }>();
/** What the page wants, by track. */
let wants = new Map<Path, Subscription>();
/** Tracks subscribed on this connection, with the options sent. */
const subscribed = new Map<Path, string>();
/** Locks this session holds: heartbeats go out for each. */
const held = new Set<Path>();
/** Next sequence number per channel and lock, as in tq::Session. */
const seqs = new Map<Path, number>();
// Group streams can overtake the state that names their track; they wait
// for it rather than being dropped (which would lose the cached GOP).
let stateWaiters: (() => void)[] = [];

function sendControl(msg: Control) {
  controlWriter?.write(encodeControl(msg)).catch(() => {});
}

function requestKeyframe(cam: Camera, reason: string) {
  const now = performance.now();
  if (now - cam.lastKeyRequest < KEYFRAME_REQUEST_MS) return;
  cam.lastKeyRequest = now;
  sendControl({ type: 'keyframe_request', track: cam.path, reason });
}

function sendDatagram(b: Uint8Array) {
  datagramWriter?.write(b).catch(() => {});
}

function nextSeq(p: Path): number {
  const seq = ((seqs.get(p) ?? 0) + 1) >>> 0;
  seqs.set(p, seq);
  return seq;
}

const broadcastAt = (index: number) => room?.broadcasts.find((b) => b.index === index);

function trackAt(broadcast: number, track: number): Track | undefined {
  const b = broadcastAt(broadcast);
  const t = b?.tracks.find((x) => x.index === track);
  return b && t ? tracks.get(path(b.name, t.info.id)) : undefined;
}

let pending: { data: DataItem[]; acks: AckItem[] } | null = null;

function flush() {
  if (!pending) return;
  post({ type: 'batch', ...pending });
  pending = null;
}

function queue(): { data: DataItem[]; acks: AckItem[] } {
  if (!pending) {
    pending = { data: [], acks: [] };
    setTimeout(flush, BATCH_MS);
  }
  return pending;
}

function queueData(item: DataItem) {
  queue().data.push(item);
}

// A new state: subscribe to tracks that appeared, drop those that went
// away, and follow which locks this session holds.
function setState(state: RoomState) {
  room = state;
  sync();
  held.clear();
  for (const b of state.broadcasts) {
    for (const l of b.locks) if (session !== null && l.holder === session) held.add(path(b.name, l.info.id));
  }
  const waiters = stateWaiters;
  stateWaiters = [];
  for (const w of waiters) w();
}

// Subscriptions follow what the page wants (plus `.status` and `.events`)
// among the tracks in the room: new ones are subscribed, changed options
// sent again, and tracks nobody wants any more unsubscribed.
function sync() {
  if (!room) return;
  const wanted = new Map<Path, { b: BroadcastState; info: TrackInfo; sub: Subscription }>();
  for (const b of room.broadcasts) {
    for (const t of b.tracks) {
      const p = path(b.name, t.info.id);
      const always = t.info.kind === 'data' && WATCHED_DATA.includes(t.info.id);
      const sub = always ? { track: p } : wants.get(p);
      if (sub) wanted.set(p, { b, info: t.info, sub });
    }
  }
  const gone: Path[] = [];
  for (const [p, t] of tracks) {
    const w = wanted.get(p);
    // Gone, or now of another kind or delivery: start over.
    const same =
      w &&
      (t instanceof Camera ? w.info.kind === 'video' : w.info.kind === 'data' && (t as DataTrack).delivery === w.info.data?.delivery);
    if (same) continue;
    t.reset();
    tracks.delete(p);
    cameras.delete(p);
    if (subscribed.delete(p)) gone.push(p);
  }
  for (const p of subscribed.keys()) {
    if (wanted.has(p)) continue;
    subscribed.delete(p);
    gone.push(p);
  }
  const fresh: Subscription[] = [];
  for (const [p, { b, info, sub }] of wanted) {
    if (!tracks.has(p)) {
      if (info.kind === 'video') {
        const cam = new Camera(p, b.name);
        for (const [id, c] of canvases) if (c.track === p) cam.addCanvas(id, c.canvas);
        cameras.set(p, cam);
        tracks.set(p, cam);
      } else {
        tracks.set(p, new DataTrack(p, info.data?.delivery ?? 'latest'));
      }
    }
    const key = JSON.stringify(sub);
    if (subscribed.get(p) !== key) {
      subscribed.set(p, key);
      fresh.push(sub);
    }
  }
  if (gone.length) sendControl({ type: 'unsubscribe', tracks: gone });
  // Subscribing again replaces a subscription's options.
  if (fresh.length) sendControl({ type: 'subscribe', tracks: fresh.map((s) => (Object.keys(s).length === 1 ? s.track : s)) });
}

function nextState(ms: number): Promise<void> {
  return new Promise((resolve) => {
    stateWaiters.push(resolve);
    setTimeout(resolve, ms);
  });
}

function hexToBytes(hex: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

async function readControl(stream: ReadableStream<Uint8Array>) {
  const r = new StreamReader(stream);
  for (;;) {
    const len = await r.readExact(4);
    if (!len) return;
    const n = new DataView(len.buffer, len.byteOffset, 4).getUint32(0);
    const json = await r.readExact(n);
    if (!json) return;
    const msg = decodeControl(json);
    switch (msg.type) {
      case 'ping':
        // The relay measures its offset to us too.
        sendControl({ type: 'pong', id: msg.id, t0_us: msg.t0_us, t1_us: nowUs() });
        continue;
      case 'pong': {
        const t3 = nowUs();
        const relayMinusMe = msg.t1_us - (msg.t0_us + t3) / 2;
        addClockSample(-relayMinusMe, t3 - msg.t0_us);
        clock.relayRttMs = (t3 - msg.t0_us) / 1000;
        continue; // not interesting to the UI
      }
      case 'welcome':
        session = msg.session;
        break;
      case 'claim_result':
        // A claim's or a release's reply; the next state confirms.
        if (msg.ok && session !== null && msg.holder === session) held.add(msg.lock);
        else if (msg.holder !== session) held.delete(msg.lock);
        break;
      default:
        break;
    }
    post({ type: 'control', msg });
  }
}

// The room's state and its events; the state brings the tracks, locks and
// each owner's clock offset.
async function readEvents(r: StreamReader) {
  const version = await r.readExact(1);
  if (!version || version[0] !== EVENTS_VERSION) throw new Error(`unsupported events stream version ${version?.[0]}`);
  for (;;) {
    const len = await r.readExact(4);
    if (!len) return;
    const n = new DataView(len.buffer, len.byteOffset, 4).getUint32(0);
    if (n === 0 || n > MAX_ROOM_MESSAGE_LEN) throw new Error(`bad room message length ${n}`);
    const json = await r.readExact(n);
    if (!json) return;
    const msg = decodeRoomMessage(json);
    if (msg.type === 'state') setState(msg.state);
    else if (msg.type !== 'event') continue; // from a newer minor
    post({ type: 'room', msg });
  }
}

// A unidirectional stream: the events stream or a group, by its type byte.
async function readStream(stream: ReadableStream<Uint8Array>) {
  const r = new StreamReader(stream);
  try {
    const type = await r.readExact(1);
    if (!type) return;
    if (type[0] === STREAM_TYPE_EVENTS) return await readEvents(r);
    if (type[0] !== STREAM_TYPE_GROUP) throw new Error(`unknown stream type ${type[0]}`);
    const rest = await r.readExact(GROUP_HEADER_FIXED - 1);
    if (!rest) return;
    const fixed = new Uint8Array(GROUP_HEADER_FIXED);
    fixed[0] = type[0];
    fixed.set(rest, 1);
    const { header, codecLen } = decodeGroupFixed(fixed);
    const codecBytes = codecLen ? await r.readExact(codecLen) : new Uint8Array();
    header.codec = new TextDecoder().decode(codecBytes ?? new Uint8Array());
    let track = trackAt(header.broadcast, header.track);
    const deadline = performance.now() + STATE_WAIT_MS;
    while (!track && performance.now() < deadline) {
      await nextState(deadline - performance.now());
      track = trackAt(header.broadcast, header.track);
    }
    if (!track || !track.startGroup(header, r)) {
      r.cancel();
      return;
    }
    for (;;) {
      const lenBytes = await r.readExact(4);
      if (!lenBytes) return;
      const len = new DataView(lenBytes.buffer, lenBytes.byteOffset, 4).getUint32(0);
      if (len < FRAME_HEADER_LEN || len > MAX_FRAME_LEN) throw new Error(`bad frame length ${len}`);
      const body = await r.readExact(len);
      if (!body) throw new Error('truncated frame');
      if (!track.current(header.group) || tracks.get(track.path) !== track) return; // superseded meanwhile
      track.frame(decodeFrameHeader(body), body.subarray(FRAME_HEADER_LEN), header.codec);
    }
  } catch {
    // Reset by the relay (late or superseded group) or cancelled by us.
  }
}

async function readStreams(t: WebTransport) {
  const reader = t.incomingUnidirectionalStreams.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    void readStream(value as ReadableStream<Uint8Array>);
  }
}

async function readDatagrams(t: WebTransport) {
  const reader = t.datagrams.readable.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    const d = decodeDatagram(value as Uint8Array);
    if (!d) continue;
    if (d.type === 'track_frame') {
      const track = trackAt(d.broadcast, d.track);
      const info = broadcastAt(d.broadcast)?.tracks.find((x) => x.index === d.track)?.info;
      if (track && info) track.frame(d.header, d.payload, info.data?.encoding ?? 'bytes');
      continue;
    }
    const rtt = (nowUs() - d.echoUs) / 1000;
    if (d.type === 'ack') {
      stopAcked(d.broadcast, d.channel, d.seq, d.status, d.detail);
      const b = broadcastAt(d.broadcast);
      const c = b?.channels.find((x) => x.index === d.channel);
      if (b && c) queue().acks.push({ channel: path(b.name, c.info.id), seq: d.seq, status: d.status, detail: d.detail, rttMs: rtt });
      teleop.ackRtt.add(rtt);
      teleop.acks[d.status] = (teleop.acks[d.status] ?? 0) + 1;
      if (d.detail) teleop.lastAckDetail = d.detail;
    } else {
      teleop.heartbeatRtt.add(rtt);
      teleop.tripped = d.tripped;
    }
  }
}

// A JSON message on `<broadcast>/<channel>`, by the room's indices; its
// sequence number, or null if it could not be sent.
function sendMessage(channel: Path, body: unknown): number | null {
  const [bname, cname] = channel.split('/');
  const b = room?.broadcasts.find((x) => x.name === bname);
  const c = b?.channels.find((x) => x.info.id === cname);
  if (!b || !c) return null;
  const seq = nextSeq(channel);
  const d = encodeJsonMessage(b.index, c.index, seq, nowUs(), body);
  if (!d) return null;
  sendDatagram(d);
  return seq;
}

// Remote stops go as datagrams, which can be lost: each is sent again until the
// robot acks one of its copies (the robot treats repeats as one press).
const STOP_RETRY_MS = 50;
const STOP_GIVE_UP_MS = 2000;
const stops = new Map<Path, { seqs: Set<number>; timer: ReturnType<typeof setInterval> }>();

function sendStop(channel: Path, body: unknown) {
  const previous = stops.get(channel);
  if (previous) clearInterval(previous.timer);
  const seqs = new Set<number>();
  const giveUp = performance.now() + STOP_GIVE_UP_MS;
  const attempt = () => {
    const seq = sendMessage(channel, body);
    if (seq !== null) seqs.add(seq);
    if (performance.now() >= giveUp) {
      clearInterval(entry.timer);
      stops.delete(channel);
      post({ type: 'stop', channel, acked: false });
    }
  };
  const entry = { seqs, timer: setInterval(attempt, STOP_RETRY_MS) };
  stops.set(channel, entry);
  attempt();
}

function stopAcked(broadcast: number, channel: number, seq: number, status: string, detail: string) {
  const b = broadcastAt(broadcast);
  const c = b?.channels.find((x) => x.index === channel);
  if (!b || !c) return;
  const p = path(b.name, c.info.id);
  const entry = stops.get(p);
  if (!entry?.seqs.has(seq)) return;
  clearInterval(entry.timer);
  stops.delete(p);
  post({ type: 'stop', channel: p, acked: true, status, detail });
}

function sendHeartbeats() {
  for (const lock of held) {
    const [bname, lname] = lock.split('/');
    const b = room?.broadcasts.find((x) => x.name === bname);
    const l = b?.locks.find((x) => x.info.id === lname);
    if (b && l) sendDatagram(encodeHeartbeat(b.index, l.index, nextSeq(lock), nowUs()));
  }
}

async function connect() {
  if (!connectArgs) return;
  const { url, certHash } = connectArgs;
  post({ type: 'status', state: 'connecting' });
  const options: WebTransportOptions & { congestionControl?: string } = {
    requireUnreliable: true,
    congestionControl: 'low-latency',
  };
  if (certHash) options.serverCertificateHashes = [{ algorithm: 'sha-256', value: hexToBytes(certHash) }];
  const t = new WebTransport(url, options);
  try {
    await t.ready;
  } catch (e) {
    post({ type: 'status', state: 'closed', detail: String(e) });
    setTimeout(connect, 1000);
    return;
  }
  transport = t;
  const bidi = await t.createBidirectionalStream();
  controlWriter = bidi.writable.getWriter();
  datagramWriter = t.datagrams.writable.getWriter();
  // The relay joins us to the room once it has our build.
  sendControl({ type: 'hello', build: WEB_BUILD });
  post({ type: 'status', state: 'connected' });

  void readControl(bidi.readable).catch(() => {});
  void readStreams(t).catch(() => {});
  void readDatagrams(t).catch(() => {});

  let pingId = 0;
  const ping = setInterval(() => sendControl({ type: 'ping', id: ++pingId, t0_us: nowUs() }), PING_MS);
  sendControl({ type: 'ping', id: 0, t0_us: nowUs() });
  const heartbeat = setInterval(sendHeartbeats, HEARTBEAT_MS);

  const closed = await t.closed.then(
    (info) => `closed (${info.closeCode ?? 0}${info.reason ? `: ${info.reason}` : ''})`,
    (e) => String(e),
  );
  clearInterval(ping);
  clearInterval(heartbeat);
  transport = null;
  controlWriter = null;
  datagramWriter = null;
  // Tracks (and their canvases) stay until a state says they are gone.
  for (const track of tracks.values()) track.reset();
  subscribed.clear();
  held.clear();
  seqs.clear();
  session = null;
  room = null;
  clockSamples.length = 0;
  teleop.tripped = null;
  post({ type: 'status', state: 'closed', detail: closed });
  setTimeout(connect, 1000);
}

setInterval(() => {
  if (!transport) return;
  const stats: Record<Path, CameraStats> = {};
  for (const [p, cam] of cameras) stats[p] = cam.stats();
  post({
    type: 'stats',
    cameras: stats,
    clock: {
      relayRttMs: clock.relayRttMs,
      offsetMs: clock.myOffsetUs === null ? null : clock.myOffsetUs / 1000,
      errorMs: clock.myErrorUs === null ? null : clock.myErrorUs / 1000,
      refreshMs: display.intervalMs,
      sensorMs,
    },
    teleop: {
      ackRtt: teleop.ackRtt.take(),
      heartbeatRtt: teleop.heartbeatRtt.take(),
      acks: { ...teleop.acks },
      lastAckDetail: teleop.lastAckDetail,
      tripped: teleop.tripped,
    },
  });
}, 500);

scope.onmessage = (e) => {
  const msg = e.data;
  switch (msg.type) {
    case 'connect':
      sensorMs = msg.sensorMs;
      connectArgs = { url: msg.url, certHash: msg.certHash };
      void connect();
      break;
    case 'canvas':
      canvases.set(msg.id, { track: msg.track, canvas: msg.canvas });
      cameras.get(msg.track)?.addCanvas(msg.id, msg.canvas);
      break;
    case 'uncanvas': {
      const c = canvases.get(msg.id);
      canvases.delete(msg.id);
      if (c) cameras.get(c.track)?.canvases.delete(msg.id);
      break;
    }
    case 'want':
      wants = new Map(msg.subs.map((s) => [s.track, s]));
      sync();
      break;
    case 'control':
      sendControl(msg.msg);
      break;
    case 'message':
      sendMessage(msg.channel, msg.body);
      break;
    case 'stop':
      sendStop(msg.channel, msg.body);
      break;
  }
};
