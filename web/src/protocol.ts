// Wire format of the tq protocol. Mirrors crates/tq-proto; keep the two in
// step, PROTOCOL included.

export const STREAM_TYPE_GROUP = 0x01;
/** Group stream header version (the framing, not the protocol). */
export const VERSION = 1;
/** The room's events stream; see crates/tq-proto/src/room.rs. */
export const STREAM_TYPE_EVENTS = 0x02;
export const EVENTS_VERSION = 1;
export const MAX_ROOM_MESSAGE_LEN = 1 << 20;

export interface ProtocolVersion {
  major: number;
  minor: number;
}

/** The protocol this build speaks; see crates/tq-proto/src/version.rs. */
export const PROTOCOL: ProtocolVersion = { major: 2, minor: 0 };
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
export const GROUP_HEADER_FIXED = 13;
export const FRAME_HEADER_LEN = 33;
export const MAX_FRAME_LEN = 16 << 20;
export const FLAG_KEY = 1 << 0;
export const FLAG_CAPTURE_FROM_SEI = 1 << 1;

export const DG_COMMAND = 0x10;
export const DG_ACK = 0x11;
export const DG_HEARTBEAT = 0x12;
export const DG_ECHO = 0x13;

export const ACK_STATUS = ['ok', 'superseded', 'rejected', 'malformed'] as const;
export type AckStatus = (typeof ACK_STATUS)[number];

export type Role = 'publisher' | 'operator' | 'viewer';

export interface GroupHeader {
  camera: number;
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

export interface CameraInfo {
  id: string;
  label: string;
  codec: string;
  width: number;
  height: number;
  fps: number;
}

export interface CameraStatus {
  id: string;
  online: boolean;
  codec_string: string | null;
  frames: number;
  groups: number;
  fps: number;
  kbps: number;
  dropped: number;
  capture_to_publisher_ms: number | null;
}

export interface TeleopStatus {
  deadman_tripped: boolean;
  reason: string;
  last_seq: number;
  commands: number;
  operator: string | null;
}

export interface QuicStats {
  rtt_ms: number;
  cwnd: number;
  lost_packets: number;
  sent_packets: number;
  congestion_events: number;
}

export interface PublisherStatus {
  publisher_id: string;
  build?: BuildInfo | null;
  cameras: CameraInfo[];
  camera_status: CameraStatus[];
  teleop: TeleopStatus;
  rtt_ms: number | null;
  clock_offset_us: number | null;
  clock_error_us?: number | null;
  quic: QuicStats | null;
}

export interface ParticipantInfo {
  session: string;
  role: Role;
  sub: string;
  build: BuildInfo;
  joined_us: number;
}

export interface TrackInfo {
  camera: CameraInfo;
  online: boolean;
}

export interface RoomState {
  room_id: string;
  publisher: PublisherStatus | null;
  /** In announce order: a track's index is the `camera` on group streams. */
  tracks: TrackInfo[];
  participants: ParticipantInfo[];
  operator: string | null;
}

export type RoomEventBody =
  | { type: 'participant_joined'; participant: ParticipantInfo }
  | { type: 'participant_left'; participant: ParticipantInfo }
  | { type: 'publisher_connected'; publisher_id: string; build: BuildInfo | null }
  | { type: 'publisher_disconnected'; publisher_id: string; reason: 'disconnected' | 'replaced' }
  | { type: 'track_published'; track: TrackInfo }
  | { type: 'track_updated'; track: TrackInfo }
  | { type: 'track_unpublished'; camera: string; reason: string }
  | { type: 'track_online'; camera: string }
  | { type: 'track_offline'; camera: string; reason: string }
  | {
      type: 'operator_changed';
      from: string | null;
      to: string | null;
      reason: 'claimed' | 'forced' | 'released' | 'left';
    }
  | { type: 'deadman_tripped'; reason: string }
  | { type: 'deadman_cleared' };

/**
 * On the events stream: the room as of event `seq`, or the next event.
 * Events at or below the last state's `seq` are never sent.
 */
export type RoomMessage =
  | { type: 'state'; seq: number; state: RoomState }
  | { type: 'event'; seq: number; ts_us: number; event: RoomEventBody };

export type Control =
  | { type: 'welcome'; session: string; role: Role; relay?: BuildInfo | null }
  | { type: 'claim_result'; ok: boolean; holder: string | null; reason: string | null }
  | { type: 'link_stats'; quic: QuicStats }
  | { type: 'hello'; build: BuildInfo }
  | { type: 'claim'; force: boolean }
  | { type: 'release' }
  | { type: 'keyframe_request'; camera: string; reason: string }
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
    header: { camera: view.getUint16(2), group: view.getBigUint64(4), codec: '' },
    codecLen: view.getUint8(12),
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

export function encodeCommand(seq: number, sentUs: number, body: unknown): Uint8Array {
  const json = encoder.encode(JSON.stringify(body));
  const out = new Uint8Array(13 + json.length);
  const view = new DataView(out.buffer);
  view.setUint8(0, DG_COMMAND);
  view.setUint32(1, seq);
  putU64(view, 5, sentUs);
  out.set(json, 13);
  return out;
}

export function encodeHeartbeat(seq: number, sentUs: number): Uint8Array {
  const out = new Uint8Array(13);
  const view = new DataView(out.buffer);
  view.setUint8(0, DG_HEARTBEAT);
  view.setUint32(1, seq);
  putU64(view, 5, sentUs);
  return out;
}

export type Datagram =
  | { type: 'ack'; seq: number; echoUs: number; status: AckStatus; detail: string }
  | { type: 'echo'; seq: number; echoUs: number; tripped: boolean };

export function decodeDatagram(b: Uint8Array): Datagram | null {
  if (b.length < 13) return null;
  const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const type = view.getUint8(0);
  const seq = view.getUint32(1);
  const echoUs = u64(view, 5);
  if (type === DG_ACK && b.length >= 14) {
    return {
      type: 'ack',
      seq,
      echoUs,
      status: ACK_STATUS[view.getUint8(13)] ?? 'malformed',
      detail: decoder.decode(b.subarray(14)),
    };
  }
  if (type === DG_ECHO) {
    return { type: 'echo', seq, echoUs, tripped: b.length > 13 && view.getUint8(13) !== 0 };
  }
  return null;
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
