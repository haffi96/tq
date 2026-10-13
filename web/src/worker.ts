// Media worker: owns the WebTransport session, reads every camera's GOP
// streams, decodes them with WebCodecs and draws each frame on its
// OffscreenCanvas the moment it is decoded. No jitter buffer: a frame is
// shown as soon as it is complete. Teleop datagrams also go out from here
// so they never wait behind the UI thread.

import { SpsPatcher } from './h264sps';
import {
  type CameraInfo,
  type Control,
  type GroupHeader,
  type Role,
  FLAG_CAPTURE_FROM_SEI,
  FLAG_KEY,
  FRAME_HEADER_LEN,
  GROUP_HEADER_FIXED,
  MAX_FRAME_LEN,
  StreamReader,
  decodeControl,
  decodeDatagram,
  decodeFrameHeader,
  decodeGroupFixed,
  encodeCommand,
  encodeControl,
  encodeHeartbeat,
  nowUs,
} from './protocol';
import { WEB_BUILD } from './version';

export type ToWorker =
  // hello: the relay reported a build, so it knows the `hello` message.
  | { type: 'connect'; url: string; certHash: string | null; role: Role; sensorMs: number; hello: boolean }
  | { type: 'canvas'; camera: string; canvas: OffscreenCanvas }
  | { type: 'control'; msg: Control }
  | { type: 'teleop'; command: string; args: unknown };

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
  | {
      type: 'stats';
      cameras: Record<string, CameraStats>;
      clock: {
        relayRttMs: number | null;
        offsetMs: number | null;
        // Error bound of the publisher -> browser clock mapping (both hops).
        syncErrorMs: number | null;
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

const scope = self as unknown as {
  postMessage(msg: FromWorker): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
};
const post = (msg: FromWorker) => scope.postMessage(msg);

const DECODE_STALL_MS = 150;
const PENDING_EXPIRY_MS = 1000;
const KEYFRAME_REQUEST_MS = 500;
const HEARTBEAT_MS = 100;

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

class Camera {
  canvas: OffscreenCanvas | null = null;
  ctx: OffscreenCanvasRenderingContext2D | null = null;
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

  constructor(readonly info: CameraInfo) {}

  setCanvas(canvas: OffscreenCanvas) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d', { alpha: false, desynchronized: true }) as OffscreenCanvasRenderingContext2D | null;
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
    console.error(`[${this.info.id}] decoder error`, e);
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.waitKey = true;
    requestKeyframe(this, 'decoder error');
  }

  frame(header: ReturnType<typeof decodeFrameHeader>, data: Uint8Array, codec: string) {
    const recvUs = nowUs();
    const key = (header.flags & FLAG_KEY) !== 0;
    const sei = (header.flags & FLAG_CAPTURE_FROM_SEI) !== 0;
    this.captureFromSei = sei;
    this.c.frames += 1;
    this.c.bytes += data.length;

    const pubOff = clock.pubOffsetUs;
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
    const canvas = this.canvas;
    if (canvas && this.ctx) {
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      this.ctx.drawImage(frame, 0, 0);
    }
    frame.close();
    this.c.decoded += 1;
    if (!info) return;
    this.w.decode.add(performance.now() - info.submitMs);
    const pubOff = clock.pubOffsetUs;
    const myOff = clock.myOffsetUs;
    if (pubOff === null || myOff === null) return;
    // Capture time on the browser clock: publisher clock -> relay -> here.
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
    return {
      codec: this.codec,
      size: this.canvas?.width ? `${this.canvas.width}x${this.canvas.height}` : '-',
      fps,
      kbps,
      ...this.c,
      decodeQueue: this.decoder?.decodeQueueSize ?? 0,
      spsPatched: this.sps.patched,
      captureFromSei: this.captureFromSei,
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

// Clock offsets (microseconds): this browser minus the relay, and the
// publisher minus the relay (reported by the relay), with their error
// bounds (half the best round trip each came from).
const clock = {
  myOffsetUs: null as number | null,
  myErrorUs: null as number | null,
  pubOffsetUs: null as number | null,
  pubErrorUs: null as number | null,
  relayRttMs: null as number | null,
};

// Same filter as tq_proto::ClockFilter: of the last CLOCK_WINDOW pings,
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
  seq: 0,
  hbSeq: 0,
  ackRtt: new Win(),
  heartbeatRtt: new Win(),
  acks: {} as Record<string, number>,
  lastAckDetail: null as string | null,
  tripped: null as boolean | null,
};

let role: Role = 'viewer';
let transport: WebTransport | null = null;
let controlWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
let datagramWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
let cameraList: CameraInfo[] = [];
const cameras = new Map<string, Camera>();
const canvases = new Map<string, OffscreenCanvas>();
let connectArgs: { url: string; certHash: string | null; hello: boolean } | null = null;
// Group streams can overtake the welcome message; they wait for the camera
// list rather than being dropped (which would lose the cached GOP).
let camerasKnown: () => void = () => {};
let camerasReady = new Promise<void>((resolve) => (camerasKnown = resolve));

function sendControl(msg: Control) {
  controlWriter?.write(encodeControl(msg)).catch(() => {});
}

function requestKeyframe(cam: Camera, reason: string) {
  const now = performance.now();
  if (now - cam.lastKeyRequest < KEYFRAME_REQUEST_MS) return;
  cam.lastKeyRequest = now;
  sendControl({ type: 'keyframe_request', camera: cam.info.id, reason });
}

function sendDatagram(b: Uint8Array) {
  datagramWriter?.write(b).catch(() => {});
}

function setCameras(list: CameraInfo[]) {
  cameraList = list;
  camerasKnown();
  for (const info of list) {
    let cam = cameras.get(info.id);
    if (!cam) {
      cam = new Camera(info);
      cameras.set(info.id, cam);
      const canvas = canvases.get(info.id);
      if (canvas) cam.setCanvas(canvas);
    }
  }
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
      case 'welcome':
      case 'state':
        clock.pubOffsetUs = msg.state.publisher?.clock_offset_us ?? null;
        clock.pubErrorUs = msg.state.publisher?.clock_error_us ?? null;
        if (msg.state.publisher) setCameras(msg.state.publisher.cameras);
        break;
      case 'pong': {
        const t3 = nowUs();
        const relayMinusMe = msg.t1_us - (msg.t0_us + t3) / 2;
        addClockSample(-relayMinusMe, t3 - msg.t0_us);
        clock.relayRttMs = (t3 - msg.t0_us) / 1000;
        continue; // not interesting to the UI
      }
      default:
        break;
    }
    post({ type: 'control', msg });
  }
}

async function readGroup(stream: ReadableStream<Uint8Array>) {
  const r = new StreamReader(stream);
  try {
    const fixed = await r.readExact(GROUP_HEADER_FIXED);
    if (!fixed) return;
    const { header, codecLen } = decodeGroupFixed(fixed);
    const codecBytes = codecLen ? await r.readExact(codecLen) : new Uint8Array();
    header.codec = new TextDecoder().decode(codecBytes ?? new Uint8Array());
    if (!cameraList[header.camera]) {
      await Promise.race([camerasReady, new Promise((resolve) => setTimeout(resolve, 2000))]);
    }
    const info = cameraList[header.camera];
    const cam = info && cameras.get(info.id);
    if (!cam || !cam.startGroup(header, r)) {
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
      if (cam.group !== header.group) return; // superseded meanwhile
      cam.frame(decodeFrameHeader(body), body.subarray(FRAME_HEADER_LEN), header.codec);
    }
  } catch {
    // Reset by the relay (late or superseded group) or cancelled by us.
  }
}

async function readGroups(t: WebTransport) {
  const reader = t.incomingUnidirectionalStreams.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    void readGroup(value as ReadableStream<Uint8Array>);
  }
}

async function readDatagrams(t: WebTransport) {
  const reader = t.datagrams.readable.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    const d = decodeDatagram(value as Uint8Array);
    if (!d) continue;
    const rtt = (nowUs() - d.echoUs) / 1000;
    if (d.type === 'ack') {
      teleop.ackRtt.add(rtt);
      teleop.acks[d.status] = (teleop.acks[d.status] ?? 0) + 1;
      if (d.detail) teleop.lastAckDetail = d.detail;
    } else {
      teleop.heartbeatRtt.add(rtt);
      teleop.tripped = d.tripped;
    }
  }
}

async function connect() {
  if (!connectArgs) return;
  const { url, certHash, hello } = connectArgs;
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
  if (hello) sendControl({ type: 'hello', build: WEB_BUILD });
  post({ type: 'status', state: 'connected' });

  void readControl(bidi.readable).catch(() => {});
  void readGroups(t).catch(() => {});
  void readDatagrams(t).catch(() => {});

  const ping = setInterval(() => sendControl({ type: 'ping', id: Date.now(), t0_us: nowUs() }), PING_MS);
  sendControl({ type: 'ping', id: 0, t0_us: nowUs() });
  const heartbeat =
    role === 'operator'
      ? setInterval(() => sendDatagram(encodeHeartbeat(++teleop.hbSeq, nowUs())), HEARTBEAT_MS)
      : null;

  const closed = await t.closed.then(
    (info) => `closed (${info.closeCode ?? 0}${info.reason ? `: ${info.reason}` : ''})`,
    (e) => String(e),
  );
  clearInterval(ping);
  if (heartbeat) clearInterval(heartbeat);
  transport = null;
  controlWriter = null;
  datagramWriter = null;
  for (const cam of cameras.values()) cam.reset();
  camerasReady = new Promise<void>((resolve) => (camerasKnown = resolve));
  cameraList = [];
  clockSamples.length = 0;
  post({ type: 'status', state: 'closed', detail: closed });
  setTimeout(connect, 1000);
}

setInterval(() => {
  if (!transport) return;
  const stats: Record<string, CameraStats> = {};
  for (const [id, cam] of cameras) stats[id] = cam.stats();
  post({
    type: 'stats',
    cameras: stats,
    clock: {
      relayRttMs: clock.relayRttMs,
      offsetMs: clock.myOffsetUs === null ? null : clock.myOffsetUs / 1000,
      syncErrorMs:
        clock.myErrorUs === null || clock.pubErrorUs === null ? null : (clock.myErrorUs + clock.pubErrorUs) / 1000,
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
      role = msg.role;
      sensorMs = msg.sensorMs;
      connectArgs = { url: msg.url, certHash: msg.certHash, hello: msg.hello };
      void connect();
      break;
    case 'canvas': {
      canvases.set(msg.camera, msg.canvas);
      cameras.get(msg.camera)?.setCanvas(msg.canvas);
      break;
    }
    case 'control':
      sendControl(msg.msg);
      break;
    case 'teleop':
      teleop.seq += 1;
      sendDatagram(encodeCommand(teleop.seq, nowUs(), { command: msg.command, args: msg.args }));
      break;
  }
};
