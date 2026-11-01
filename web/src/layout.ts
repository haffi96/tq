// Layouts: a grid of widgets, each bound to the room's data by path. The
// spec is docs/layout-1.md; this file validates it, places widgets and
// makes the presets.

import type { Path, RoomState } from './protocol';
import { path } from './protocol';

export const LAYOUT_VERSION = 1;

/** Column, row, width, height, in grid cells. */
export type At = [number, number, number, number];

/**
 * What a widget shows or drives. `track`, `channel` and `lock` are paths
 * (`<broadcast>/<id>`) or bare ids, which resolve against `broadcast`, then
 * the layout's `broadcast`, then the first broadcast in the room that has one.
 */
export interface Bind {
  broadcast?: string;
  track?: string;
  /** A field in a JSON data track's messages: `battery.voltage`, `cells.0`. */
  path?: string;
  channel?: string;
  lock?: string;
}

export interface WidgetSpec {
  id: string;
  type: string;
  at: At;
  bind?: Bind;
  props?: Record<string, unknown>;
  /** Cannot be moved, resized or removed in the editor. */
  pinned?: boolean;
}

export interface Layout {
  version: 1;
  name?: string;
  /** Broadcast that bare ids resolve against. */
  broadcast?: string;
  grid: { cols: number; row_height: number };
  widgets: WidgetSpec[];
}

const int = (v: unknown, min: number, max: number): v is number =>
  typeof v === 'number' && Number.isInteger(v) && v >= min && v <= max;
const str = (v: unknown): v is string => typeof v === 'string' && v.length > 0;
const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

/**
 * A layout from JSON, or the problems that stop it loading. Widget types
 * this build does not know are kept (they render as a placeholder), so a
 * layout made by a newer app still loads.
 */
export function parseLayout(v: unknown): { layout: Layout | null; errors: string[] } {
  const errors: string[] = [];
  if (!isObj(v)) return { layout: null, errors: ['not a JSON object'] };
  if (v.version !== LAYOUT_VERSION) errors.push(`version must be ${LAYOUT_VERSION}, not ${JSON.stringify(v.version)}`);
  const grid = v.grid;
  if (!isObj(grid) || !int(grid.cols, 1, 48) || !int(grid.row_height, 8, 400)) {
    errors.push('grid needs integer cols (1-48) and row_height (8-400)');
  }
  if (v.name !== undefined && typeof v.name !== 'string') errors.push('name must be a string');
  if (v.broadcast !== undefined && !str(v.broadcast)) errors.push('broadcast must be a string');
  if (!Array.isArray(v.widgets)) errors.push('widgets must be an array');
  const cols = isObj(grid) && int(grid.cols, 1, 48) ? grid.cols : 48;
  const ids = new Set<string>();
  for (const [i, w] of (Array.isArray(v.widgets) ? v.widgets : []).entries()) {
    const at = `widgets[${i}]`;
    if (!isObj(w)) {
      errors.push(`${at} is not an object`);
      continue;
    }
    if (!str(w.id)) errors.push(`${at}.id must be a string`);
    else if (ids.has(w.id)) errors.push(`${at}.id "${w.id}" is used twice`);
    else ids.add(w.id);
    if (!str(w.type)) errors.push(`${at}.type must be a string`);
    const a = w.at;
    if (
      !Array.isArray(a) ||
      a.length !== 4 ||
      !int(a[0], 0, cols - 1) ||
      !int(a[1], 0, 10_000) ||
      !int(a[2], 1, cols) ||
      !int(a[3], 1, 1_000) ||
      a[0] + a[2] > cols
    ) {
      errors.push(`${at}.at must be [col, row, width, height] inside ${cols} columns`);
    }
    if (w.bind !== undefined) {
      if (!isObj(w.bind)) errors.push(`${at}.bind must be an object`);
      else
        for (const [k, b] of Object.entries(w.bind)) {
          if (!['broadcast', 'track', 'path', 'channel', 'lock'].includes(k)) errors.push(`${at}.bind.${k} is not a binding`);
          else if (!str(b)) errors.push(`${at}.bind.${k} must be a string`);
        }
    }
    if (w.props !== undefined && !isObj(w.props)) errors.push(`${at}.props must be an object`);
    if (w.pinned !== undefined && typeof w.pinned !== 'boolean') errors.push(`${at}.pinned must be true or false`);
  }
  return errors.length ? { layout: null, errors } : { layout: v as unknown as Layout, errors };
}

const overlaps = (a: At, b: At) => a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3];

/**
 * Moves widgets down until none overlap, `first` keeping its place (the one
 * just dropped or resized). Pinned widgets never move.
 */
export function settle(widgets: WidgetSpec[], first?: string) {
  const order = [...widgets].sort((a, b) => {
    const rank = (w: WidgetSpec) => (w.pinned ? 0 : w.id === first ? 1 : 2);
    return rank(a) - rank(b) || a.at[1] - b.at[1] || a.at[0] - b.at[0];
  });
  const placed: WidgetSpec[] = [];
  for (const w of order) {
    if (!w.pinned) {
      for (let hit = placed.find((p) => overlaps(p.at, w.at)); hit; hit = placed.find((p) => overlaps(p.at, w.at))) {
        w.at = [w.at[0], hit.at[1] + hit.at[3], w.at[2], w.at[3]];
      }
    }
    placed.push(w);
  }
  // Then everything floats up as far as it can, top first.
  const done: WidgetSpec[] = widgets.filter((w) => w.pinned);
  for (const w of [...widgets].filter((x) => !x.pinned).sort((a, b) => a.at[1] - b.at[1] || a.at[0] - b.at[0])) {
    while (w.at[1] > 0) {
      const up: At = [w.at[0], w.at[1] - 1, w.at[2], w.at[3]];
      if (done.some((p) => overlaps(p.at, up))) break;
      w.at = up;
    }
    done.push(w);
  }
}

/** The first free spot for a widget `w` cells wide and `h` high. */
export function freeSpot(layout: Layout, w: number, h: number): At {
  const cols = layout.grid.cols;
  w = Math.min(w, cols);
  for (let y = 0; ; y += 1) {
    for (let x = 0; x + w <= cols; x += 1) {
      const at: At = [x, y, w, h];
      if (!layout.widgets.some((o) => overlaps(o.at, at))) return at;
    }
  }
}

// --- presets ---------------------------------------------------------------

const DRIVE = 'drive';
const STOP = 'stop';

/**
 * The layout an app starts with when the user has not saved one: every
 * broadcast's video, its status and a table of each data track, and for an
 * operator the teleop controls (lock, drive) where a broadcast has a drive
 * lock. Ids are stable, so remaking it as the room changes keeps widgets
 * (and their video) in place.
 */
export function presetLayout(state: RoomState, operator: boolean): Layout {
  const layout: Layout = { version: 1, name: operator ? 'teleop' : 'monitor', grid: { cols: 12, row_height: 40 }, widgets: [] };
  const add = (id: string, type: string, w: number, h: number, bind?: Bind, props?: Record<string, unknown>) => {
    const at = freeSpot(layout, w, h);
    const spec: WidgetSpec = { id, type, at };
    if (bind) spec.bind = bind;
    if (props) spec.props = props;
    layout.widgets.push(spec);
  };
  let firstVideo: Path | null = null;
  for (const b of state.broadcasts) {
    for (const t of b.tracks) {
      if (t.info.kind !== 'video') continue;
      const p = path(b.name, t.info.id);
      firstVideo ??= p;
      add(`video:${p}`, 'video', 6, 9, { track: p });
    }
  }
  for (const b of state.broadcasts) {
    const teleop = b.locks.some((l) => l.info.id === DRIVE) && b.channels.some((c) => c.info.id === DRIVE);
    if (operator && teleop) {
      add(`lock:${b.name}`, 'lock', 3, 4, { lock: path(b.name, DRIVE) });
      add(`drive:${b.name}`, 'drive', 3, 4, { channel: path(b.name, DRIVE), lock: path(b.name, DRIVE) }, { rate_hz: 20 });
    }
    if (teleop || b.channels.some((c) => c.info.id === STOP)) add(`status:${b.name}`, 'status', 3, 4, { broadcast: b.name });
    for (const t of b.tracks) {
      if (t.info.kind !== 'data' || t.info.id.startsWith('.')) continue;
      add(`values:${b.name}/${t.info.id}`, 'values', 3, 4, { track: path(b.name, t.info.id) });
    }
  }
  if (firstVideo) add('latency', 'latency', 3, 4, { track: firstVideo });
  add('log', 'log', 12, 4);
  return layout;
}

/** Ids, types and bindings of a layout: whether two presets differ. */
export const layoutKey = (l: Layout) => JSON.stringify(l.widgets.map((w) => [w.id, w.type, w.bind, w.props]));
