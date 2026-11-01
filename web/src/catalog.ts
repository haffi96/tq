// The data catalog: everything in the room a widget can bind to (tracks,
// with the fields seen so far on JSON data tracks; channels; locks), and
// what this session's permissions allow with each.

import type { Bind } from './layout';
import type { ChannelInfo, LockInfo, Path, Perms, RoomState, TrackInfo } from './protocol';
import { path } from './protocol';

export type FieldType = 'number' | 'boolean' | 'string';

export interface Field {
  type: FieldType;
}

/** A field of a JSON value: `a.b.0` walks objects and arrays. */
export function getField(value: unknown, field: string): unknown {
  let v = value;
  for (const k of field.split('.')) {
    if (typeof v !== 'object' || v === null) return undefined;
    v = (v as Record<string, unknown>)[k];
  }
  return v;
}

// Leaves of a JSON value, as dotted paths. Bounded: a huge message adds at
// most MAX_FIELDS fields.
const MAX_FIELDS = 256;
function leaves(v: unknown, prefix: string, out: Map<string, Field>, depth = 0) {
  if (out.size >= MAX_FIELDS || depth > 6) return;
  if (typeof v === 'number' || typeof v === 'boolean' || typeof v === 'string') {
    if (prefix) out.set(prefix, { type: typeof v as FieldType });
    return;
  }
  if (typeof v !== 'object' || v === null) return;
  for (const [k, x] of Object.entries(v)) leaves(x, prefix ? `${prefix}.${k}` : k, out, depth + 1);
}

/** `verb:pattern` allows `p` if each segment matches (`*` matches any). */
export function allowed(perms: Perms, verb: string, p: Path): boolean {
  const segs = p.split('/');
  return perms.some((perm) => {
    const i = perm.indexOf(':');
    if (i < 0 || perm.slice(0, i) !== verb) return false;
    const pat = perm.slice(i + 1).split('/');
    return pat.length === segs.length && pat.every((s, j) => s === '*' || s === segs[j]);
  });
}

export interface Entry<T> {
  path: Path;
  broadcast: string;
  info: T;
}

/** How often a track's messages are scanned for new fields. */
const SCAN_MS = 1000;

export class Catalog {
  state: RoomState | null = null;
  perms: Perms = [];
  private fieldsOf = new Map<Path, Map<string, Field>>();
  private scanned = new Map<Path, number>();
  /** Bumped when the set of fields changes (the editor's pickers redraw). */
  version = 0;

  /** A data track's message: new fields join the catalog (at most once a second per track). */
  observe(track: Path, value: unknown, nowMs: number) {
    if (nowMs - (this.scanned.get(track) ?? -Infinity) < SCAN_MS) return;
    this.scanned.set(track, nowMs);
    let known = this.fieldsOf.get(track);
    if (!known) this.fieldsOf.set(track, (known = new Map()));
    const seen = new Map<string, Field>();
    leaves(value, '', seen);
    for (const [k, f] of seen) {
      if (known.has(k) || known.size >= MAX_FIELDS) continue;
      known.set(k, f);
      this.version += 1;
    }
  }

  fields(track: Path): [string, Field][] {
    return [...(this.fieldsOf.get(track) ?? [])].sort(([a], [b]) => a.localeCompare(b));
  }

  tracks(kind?: TrackInfo['kind']): Entry<TrackInfo>[] {
    return (this.state?.broadcasts ?? []).flatMap((b) =>
      b.tracks.filter((t) => !kind || t.info.kind === kind).map((t) => ({ path: path(b.name, t.info.id), broadcast: b.name, info: t.info })),
    );
  }

  channels(): Entry<ChannelInfo>[] {
    return (this.state?.broadcasts ?? []).flatMap((b) =>
      b.channels.map((c) => ({ path: path(b.name, c.info.id), broadcast: b.name, info: c.info })),
    );
  }

  locks(): Entry<LockInfo>[] {
    return (this.state?.broadcasts ?? []).flatMap((b) =>
      b.locks.map((l) => ({ path: path(b.name, l.info.id), broadcast: b.name, info: l.info })),
    );
  }

  broadcasts(): string[] {
    return (this.state?.broadcasts ?? []).map((b) => b.name);
  }

  track(p: Path): TrackInfo | undefined {
    return this.tracks().find((t) => t.path === p)?.info;
  }

  channel(p: Path): ChannelInfo | undefined {
    return this.channels().find((c) => c.path === p)?.info;
  }

  can(verb: 'sub' | 'write' | 'lock' | 'force', p: Path): boolean {
    return allowed(this.perms, verb, p);
  }

  /**
   * A binding as a path: `rover/front` as is; a bare `front` in the
   * binding's broadcast, else the layout's, else the first broadcast in the
   * room with one. Null while nothing in the room matches a bare id.
   */
  resolve(kind: 'track' | 'channel' | 'lock', bind: Bind | undefined, layoutBroadcast?: string): Path | null {
    const ref = bind?.[kind];
    if (!ref) return null;
    if (ref.includes('/')) return ref;
    const b = bind?.broadcast ?? layoutBroadcast;
    if (b) return path(b, ref);
    const list = kind === 'track' ? this.tracks() : kind === 'channel' ? this.channels() : this.locks();
    return list.find((e) => e.path === path(e.broadcast, ref))?.path ?? null;
  }
}
