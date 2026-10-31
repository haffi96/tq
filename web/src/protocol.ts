// Wire format of the tq protocol. Mirrors crates/tq-wire; keep the two in
// step, PROTOCOL included.

export const STREAM_TYPE_GROUP = 0x01;
/** Group stream header version (the framing, not the protocol). */
export const VERSION = 2;
/** The room's events stream; see crates/tq-wire/src/room.rs. */
export const STREAM_TYPE_EVENTS = 0x02;
export const EVENTS_VERSION = 2;
export const MAX_ROOM_MESSAGE_LEN = 1 << 20;

export interface ProtocolVersion {
  major: number;
  minor: number;
}

/** The protocol this build speaks; see crates/tq-wire/src/version.rs. */
export const PROTOCOL: ProtocolVersion = { major: 3, minor: 0 };
/** What a peer without build info speaks. */
export const PROTOCOL_LEGACY: ProtocolVersion = { major: 1, minor: 0 };

export interface BuildInfo {
  component: string;
  version: string;
  git_sha: string;
  protocol: ProtocolVersion;
}

export type Compat = 'same' | 'minor_differs' | 'incompatible';

export function compat(peer: BuildInfo | null | undefined): Compat {
  const p = peer?.protocol ?? PROTOCOL_LEGACY;
  if (p.major !== PROTOCOL.major) return 'incompatible';
  return p.minor === PROTOCOL.minor ? 'same' : 'minor_differs';
}

/** `0.3.1 (a1b2c3d4e5)`. */
export function shortBuild(b: BuildInfo): string {
  return b.git_sha && b.git_sha !== 'unknown' ? `${b.version} (${b.git_sha})` : b.version;
}

export function mismatch(name: string, peer: BuildInfo | null | undefined): string {
  const mine = `tq-web speaks ${PROTOCOL.major}.${PROTOCOL.minor}`;
  if (!peer) return `${name} predates version reporting (protocol 1.0), ${mine}`;
  return `${name} ${shortBuild(peer)} speaks protocol ${peer.protocol.major}.${peer.protocol.minor}, ${mine}`;
}

/** type, version, broadcast, track, group, codec_len. */
export const GROUP_HEADER_FIXED = 1 + 1 + 2 + 2 + 8 + 1;
export const FRAME_HEADER_LEN = 33;
export const MAX_FRAME_LEN = 16 << 20;
export const FLAG_KEY = 1 << 0;
export const FLAG_CAPTURE_FROM_SEI = 1 << 1;

export const DG_MESSAGE = 0x20;
export const DG_ACK = 0x21;
export const DG_HEARTBEAT = 0x22;
export const DG_ECHO = 0x23;
export const DG_TRACK_FRAME = 0x24;
/** Bytes before a message's payload: type sid broadcast channel seq sent_us. */
export const MESSAGE_HEADER_LEN = 1 + 4 + 2 + 2 + 4 + 8;
/** Bytes before a track frame's payload. */
export const TRACK_FRAME_HEADER_LEN = 1 + 2 + 2 + 4 + 1 + 8 + 8 + 8;
/** Largest datagram a sender may write. */
export const MAX_DATAGRAM = 1100;
/** Echo flag: the lock's watchdog has tripped. */
export const ECHO_TRIPPED = 1 << 0;

export const ACK_STATUS = ['ok', 'superseded', 'rejected', 'malformed'] as const;
export type AckStatus = (typeof ACK_STATUS)[number];

/** `<broadcast>/<name>`: a track, channel or lock. */
export type Path = string;
export const path = (broadcast: string, name: string): Path => `${broadcast}/${name}`;

/** `verb:pattern` strings, e.g. `lock:rover/drive`. */
export type Perms = string[];

export interface GroupHeader {
  /** The room's broadcast index. */
  broadcast: number;
  /** Position in the broadcast's track list. */
  track: number;
  group: bigint;
  codec: string;
}

export interface FrameHeader {
  index: number;
  flags: number;
  captureUs: number;
  publisherUs: number;
  relayUs: number;
  frameId: number;
}

// --- broadcasts ----------------------------------------------------------

export type TrackKind = 'video' | 'data';
export type Delivery = 'latest' | 'stream' | 'datagram';

export interface VideoInfo {
  /** `h264`, `h265`, or `auto` until the source has said. */
  codec: string;
  width: number;
  height: number;
  fps: number;
}

export interface DataInfo {
  encoding: string;
  delivery: Delivery;
  rate_hz?: number;
  window_ms?: number;
}

export interface TrackInfo {
  id: string;
  kind: TrackKind;
  label: string;
  priority?: number;
  video?: VideoInfo;
  data?: DataInfo;
  meta?: unknown;
}

export interface ChannelInfo {
  id: string;
  label: string;
  lock?: string;
  encoding: string;
  schema?: unknown;
}

export interface LockInfo {
  id: string;
  label: string;
  watchdog?: { heartbeat_loss_ms: number; message_stale_ms: number };
}

export interface QuicStats {
  rtt_ms: number;
  cwnd: number;
  lost_packets: number;
  sent_packets: number;
  congestion_events: number;
}

export interface TrackStats {
  frames: number;
  groups: number;
  fps: number;
  kbps: number;
  dropped: number;
  capture_to_publisher_ms: number | null;
  codec_string: string | null;
  subscribers: number;
}

export interface TrackState {
  /** Position in the advertisement: the `track` on group streams. */
  index: number;
  info: TrackInfo;
  online: boolean;
  stats: TrackStats;
}

export interface ChannelState {
  index: number;
  info: ChannelInfo;
}

export interface LockState {
  index: number;
  info: LockInfo;
  /** Session holding it. */
  holder: string | null;
}

export interface BroadcastState {
  /** The room's index: `broadcast` on group streams and datagrams. */
  index: number;
  name: string;
  /** The owning session, while connected. */
  owner: string | null;
  owner_sid: number | null;
  build: BuildInfo | null;
  rtt_ms: number | null;
  /** Owner clock minus relay clock. */
  clock_offset_us: number | null;
  clock_error_us: number | null;
  quic: QuicStats | null;
  tracks: TrackState[];
  channels: ChannelState[];
  locks: LockState[];
}

export interface ParticipantInfo {
  session: string;
  sid: number;
  sub: string;
  perms: Perms;
  build: BuildInfo;
  joined_us: number;
}

export interface RoomState {
  room_id: string;
  broadcasts: BroadcastState[];
  participants: ParticipantInfo[];
}

/** What crates/tq-teleop publishes on a broadcast's `.status` track. */
export interface TeleopStatus {
  tripped: boolean;
  reason: string;
  holder: string | null;
  commands: number;
  last_seq: number;
  estops: number;
}

export interface RobotStatus {
  teleop?: TeleopStatus;
  cameras?: { id: string; frames: number; codec: string | null }[];
}

export type RoomEventBody =
  | { type: 'participant_joined'; participant: ParticipantInfo }
  | { type: 'participant_left'; participant: ParticipantInfo }
  | { type: 'broadcast_started'; broadcast: string; owner: string; build: BuildInfo | null }
  | { type: 'broadcast_stopped'; broadcast: string; reason: 'disconnected' | 'replaced' | 'unadvertised' }
  | { type: 'broadcast_removed'; broadcast: string }
  | { type: 'track_published'; track: Path; info: TrackInfo }
  | { type: 'track_updated'; track: Path; info: TrackInfo }
  | { type: 'track_unpublished'; track: Path }
  | { type: 'track_online'; track: Path }
  | { type: 'track_offline'; track: Path; reason: string }
  | {
      type: 'lock_changed';
      lock: Path;
      from: string | null;
      to: string | null;
      reason: 'claimed' | 'forced' | 'released' | 'left';
    };

/**
 * On the events stream: the room as of event `seq`, or the next event.
 * Events at or below the last state's `seq` are never sent.
 */
export type RoomMessage =
  | { type: 'state'; seq: number; state: RoomState }
  | { type: 'event'; seq: number; ts_us: number; event: RoomEventBody };

export interface Subscription {
  track: Path;
  max_rate_hz?: number;
  policy?: 'realtime' | 'smooth';
}

export type Control =
  | { type: 'hello'; build: BuildInfo }
  | { type: 'welcome'; session: string; sid: number; perms: Perms; relay: BuildInfo }
  | { type: 'subscribe'; tracks: (Path | Subscription)[] }
  | { type: 'unsubscribe'; tracks: Path[] }
  | { type: 'subscribe_result'; track: Path; ok: boolean; reason?: string }
  | { type: 'keyframe_request'; track: Path; reason: string }
  | { type: 'claim'; lock: Path; force: boolean }
  | { type: 'release'; lock: Path }
  | { type: 'claim_result'; lock: Path; ok: boolean; holder: string | null; reason?: string }
  | { type: 'link_stats'; quic: QuicStats }
  | { type: 'ping'; id: number; t0_us: number }
  | { type: 'pong'; id: number; t0_us: number; t1_us: number }
  | { type: 'error'; message: string };

export function nowUs(): number {
  return Math.round((performance.timeOrigin + performance.now()) * 1000);
}

// u64 fields are read as Numbers: microsecond timestamps stay below 2^53
// for the next couple of centuries.
function u64(view: DataView, offset: number): number {
  return view.getUint32(offset) * 2 ** 32 + view.getUint32(offset + 4);
}

function putU64(view: DataView, offset: number, value: number) {
  view.setUint32(offset, Math.floor(value / 2 ** 32));
  view.setUint32(offset + 4, value >>> 0);
}

export function decodeGroupFixed(b: Uint8Array): { header: GroupHeader; codecLen: number } {
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const type = view.getUint8(0);
  if (type !== STREAM_TYPE_GROUP) throw new Error(`unknown stream type ${type}`);
  const version = view.getUint8(1);
  if (version !== VERSION) throw new Error(`unsupported version ${version}`);
  return {
    header: { broadcast: view.getUint16(2), track: view.getUint16(4), group: view.getBigUint64(6), codec: '' },
    codecLen: view.getUint8(14),
  };
}

export function decodeFrameHeader(b: Uint8Array): FrameHeader {
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    index: view.getUint32(0),
    flags: view.getUint8(4),
    captureUs: u64(view, 5),
    publisherUs: u64(view, 13),
    relayUs: u64(view, 21),
    frameId: view.getUint32(29),
  };
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export function encodeControl(msg: Control): Uint8Array {
  const json = encoder.encode(JSON.stringify(msg));
  const out = new Uint8Array(4 + json.length);
  new DataView(out.buffer).setUint32(0, json.length);
  out.set(json, 4);
  return out;
}

export function decodeControl(json: Uint8Array): Control {
  return JSON.parse(decoder.decode(json)) as Control;
}

export function decodeRoomMessage(json: Uint8Array): RoomMessage {
  return JSON.parse(decoder.decode(json)) as RoomMessage;
}

export function decodeJson(b: Uint8Array): unknown {
  return JSON.parse(decoder.decode(b));
}

// type sid broadcast id seq time: the prefix of datagrams 0x20-0x23.
// Clients always write sid 0; the relay fills it in.
function route(out: Uint8Array, type: number, broadcast: number, id: number, seq: number, us: number): DataView {
  const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
  view.setUint8(0, type);
  view.setUint32(1, 0);
  view.setUint16(5, broadcast);
  view.setUint16(7, id);
  view.setUint32(9, seq);
  putU64(view, 13, us);
  return view;
}

/** A channel message (0x20), or null if it does not fit in a datagram. */
export function encodeMessage(
  broadcast: number,
  channel: number,
  seq: number,
  sentUs: number,
  payload: Uint8Array,
): Uint8Array | null {
  if (MESSAGE_HEADER_LEN + payload.length > MAX_DATAGRAM) return null;
  const out = new Uint8Array(MESSAGE_HEADER_LEN + payload.length);
  route(out, DG_MESSAGE, broadcast, channel, seq, sentUs);
  out.set(payload, MESSAGE_HEADER_LEN);
  return out;
}

export function encodeJsonMessage(broadcast: number, channel: number, seq: number, sentUs: number, body: unknown) {
  return encodeMessage(broadcast, channel, seq, sentUs, encoder.encode(JSON.stringify(body)));
}

/** A lock heartbeat (0x22). */
export function encodeHeartbeat(broadcast: number, lock: number, seq: number, sentUs: number): Uint8Array {
  const out = new Uint8Array(MESSAGE_HEADER_LEN);
  route(out, DG_HEARTBEAT, broadcast, lock, seq, sentUs);
  return out;
}

export type Datagram =
  | { type: 'ack'; broadcast: number; channel: number; seq: number; echoUs: number; status: AckStatus; detail: string }
  | { type: 'echo'; broadcast: number; lock: number; seq: number; echoUs: number; tripped: boolean }
  | {
      type: 'track_frame';
      broadcast: number;
      track: number;
      header: FrameHeader;
      payload: Uint8Array;
    };

export function decodeDatagram(b: Uint8Array): Datagram | null {
  if (b.length < 1) return null;
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const type = view.getUint8(0);
  if (type === DG_TRACK_FRAME) {
    if (b.length < TRACK_FRAME_HEADER_LEN) return null;
    return {
      type: 'track_frame',
      broadcast: view.getUint16(1),
      track: view.getUint16(3),
      header: {
        index: 0,
        frameId: view.getUint32(5),
        flags: view.getUint8(9),
        captureUs: u64(view, 10),
        publisherUs: u64(view, 18),
        relayUs: u64(view, 26),
      },
      payload: b.subarray(TRACK_FRAME_HEADER_LEN),
    };
  }
  if ((type !== DG_ACK && type !== DG_ECHO) || b.length < MESSAGE_HEADER_LEN + 1) return null;
  const broadcast = view.getUint16(5);
  const id = view.getUint16(7);
  const seq = view.getUint32(9);
  const echoUs = u64(view, 13);
  const extra = view.getUint8(MESSAGE_HEADER_LEN);
  if (type === DG_ACK) {
    return {
      type: 'ack',
      broadcast,
      channel: id,
      seq,
      echoUs,
      status: ACK_STATUS[extra] ?? 'malformed',
      detail: decoder.decode(b.subarray(MESSAGE_HEADER_LEN + 1)),
    };
  }
  return { type: 'echo', broadcast, lock: id, seq, echoUs, tripped: (extra & ECHO_TRIPPED) !== 0 };
}

// Buffered reader over a WebTransport ReadableStream with exact reads.
export class StreamReader {
  private reader: ReadableStreamDefaultReader<Uint8Array>;
  private chunks: Uint8Array[] = [];
  private buffered = 0;

  constructor(stream: ReadableStream<Uint8Array>) {
    this.reader = stream.getReader();
  }

  // Exactly n bytes, or null if the stream ended cleanly before the first
  // byte. Throws if it ends in the middle or was reset.
  async readExact(n: number): Promise<Uint8Array | null> {
    while (this.buffered < n) {
      const { value, done } = await this.reader.read();
      if (done) {
        if (this.buffered === 0) return null;
        throw new Error('stream ended mid-message');
      }
      if (value.length) {
        this.chunks.push(value);
        this.buffered += value.length;
      }
    }
    // Fast path: the first chunk holds it all.
    const first = this.chunks[0]!;
    if (first.length >= n) {
      const out = first.subarray(0, n);
      if (first.length === n) this.chunks.shift();
      else this.chunks[0] = first.subarray(n);
      this.buffered -= n;
      return out;
    }
    const out = new Uint8Array(n);
    let filled = 0;
    while (filled < n) {
      const c = this.chunks[0]!;
      const take = Math.min(c.length, n - filled);
      out.set(c.subarray(0, take), filled);
      filled += take;
      if (take === c.length) this.chunks.shift();
      else this.chunks[0] = c.subarray(take);
    }
    this.buffered -= n;
    return out;
  }

  cancel() {
    this.reader.cancel().catch(() => {});
  }
}
