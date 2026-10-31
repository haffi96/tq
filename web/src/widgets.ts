// Widget types. Each declares what it binds to, its props (the editor's
// form) and its cost, and renders into its body. Widgets take data at its
// arrival rate and draw at most once per display frame: `data` only stores
// and marks the widget dirty, `draw` runs on the next animation frame.
// Video never goes through here: the worker draws it straight onto the
// widget's canvas, so no widget can delay a frame.

import { type Catalog, getField } from './catalog';
import type { WidgetSpec } from './layout';
import type { Path, RobotStatus, RoomState, Subscription, TrackInfo } from './protocol';
import type { AckItem, CameraStats, DataItem, ToWorker, Window } from './worker';

/** What the page gives widgets. */
export interface App {
  send(msg: ToWorker, transfer?: Transferable[]): void;
  readonly catalog: Catalog;
  readonly session: string | null;
  readonly state: RoomState | null;
  readonly operator: boolean;
  /** "you", or who holds a session. */
  who(session: string | null): string;
  readonly statuses: Map<string, RobotStatus>;
  readonly teleop: TeleopStats | null;
  readonly logLines: readonly string[];
  /** Claim (again, if held: that re-arms after a remote stop), force or release. */
  lock(lock: Path, action: 'claim' | 'force' | 'release'): void;
  /** Throttle and steer from the keyboard (WASD) and the first gamepad, or null when idle. */
  driveInput(source: string): { throttle: number; steer: number } | null;
}

export interface TeleopStats {
  ackRtt: Window;
  heartbeatRtt: Window;
  acks: Record<string, number>;
  lastAckDetail: string | null;
  tripped: boolean | null;
}

export type BindKind = 'video' | 'data' | 'field' | 'channel' | 'lock' | 'broadcast';

export interface PropDef {
  key: string;
  label: string;
  kind: 'number' | 'text' | 'bool' | 'json' | 'select';
  options?: string[];
  default?: unknown;
  /** Shown under the field. */
  help?: string;
}

/**
 * What a widget costs. `framePath`: it sits between a frame arriving and
 * the frame on screen (only video does, and it is drawn in the worker).
 */
export interface Cost {
  decode: boolean;
  redraw: string;
  framePath: boolean;
}

export interface Def {
  type: string;
  label: string;
  /** Default size in cells. */
  size: [number, number];
  /** Bindings the editor offers; `optional` ones may be left empty. */
  binds: BindKind[];
  optional?: BindKind[];
  props: PropDef[];
  cost: Cost;
  /** Sends messages or takes locks: needs write or lock permissions. */
  writes?: boolean;
  create(w: Host): Impl;
}

export interface Impl {
  /** Tracks to subscribe while the widget is visible. */
  wants?(): Subscription[];
  data?(item: DataItem): void;
  ack?(a: AckItem): void;
  camera?(s: CameraStats): void;
  /** The room's state (or the page's) changed. */
  update?(): void;
  /** On an animation frame after `dirty()`. */
  draw?(nowMs: number): void;
  /** Once a second. */
  slow?(nowMs: number): void;
  /** A remote stop went out: stop sending. */
  stopped?(): void;
  destroy?(): void;
}

/** A widget on the board, as its implementation sees it. */
export interface Host {
  readonly spec: WidgetSpec;
  readonly app: App;
  readonly body: HTMLElement;
  /** Buttons go here (the title bar). */
  readonly tools: HTMLElement;
  readonly root: HTMLElement;
  /** Unique per instance (canvas ids). */
  readonly uid: string;
  track: Path | null;
  channel: Path | null;
  lock: Path | null;
  broadcast: string | null;
  prop<T>(key: string, fallback: T): T;
  dirty(): void;
}

export const el = <K extends keyof HTMLElementTagNameMap>(tag: K, cls?: string, text?: string) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

export const fmt = (v: number | null | undefined, digits = 0) => (v === null || v === undefined || !Number.isFinite(v) ? '–' : v.toFixed(digits));
const win = (w: Window) => (w.avg === null ? '–' : `${w.avg.toFixed(1)}/${w.max!.toFixed(1)}`);
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null);
const optRate = (hz: number | null): Partial<Subscription> => (hz && hz > 0 ? { max_rate_hz: hz } : {});

/** A field's unit, from the track's `meta.units` (as tq-edge's config sets it). */
function unitOf(info: TrackInfo | undefined, field: string): string {
  const units = (info?.meta as { units?: Record<string, string> } | undefined)?.units;
  return units?.[field] ?? '';
}

function ackText(a: AckItem | null): string {
  if (!a) return '';
  return a.status === 'ok' ? `ok · ${a.rttMs.toFixed(0)} ms` : `${a.status}${a.detail ? `: ${a.detail}` : ''}`;
}

/** Why a widget that writes cannot, or null. */
function cannotWrite(w: Host): string | null {
  const { app } = w;
  if (!app.operator) return 'viewers cannot send';
  if (w.channel) {
    if (!app.catalog.channel(w.channel)) return `no channel ${w.channel}`;
    if (!app.catalog.can('write', w.channel)) return `no permission to write ${w.channel}`;
    const lock = app.catalog.channel(w.channel)?.lock;
    if (lock) {
      const lp = `${w.channel.split('/')[0]}/${lock}`;
      const holder = app.state?.broadcasts.find((b) => `${b.name}/${lock}` === lp)?.locks.find((l) => l.info.id === lock)?.holder;
      if (holder !== app.session || !app.session) return `needs the ${lp} lock`;
    }
  }
  return null;
}

// --- video ------------------------------------------------------------------

const video: Def = {
  type: 'video',
  label: 'Video',
  size: [6, 9],
  binds: ['video'],
  props: [
    { key: 'hud', label: 'Latency HUD', kind: 'select', options: ['full', 'compact', 'off'], default: 'full' },
  ],
  cost: { decode: true, redraw: 'every frame, in the worker', framePath: true },
  create(w) {
    const canvas = el('canvas');
    const overlay = el('pre', 'overlay', 'waiting for keyframe…');
    w.body.classList.add('video');
    w.body.append(canvas, overlay);
    const key = el('button', 'small', 'keyframe');
    key.title = 'Ask the broadcast owner for a keyframe';
    key.onclick = () => w.track && w.app.send({ type: 'control', msg: { type: 'keyframe_request', track: w.track, reason: 'user' } });
    w.tools.append(key);
    const offscreen = canvas.transferControlToOffscreen();
    let sent = false;
    const attach = () => {
      if (sent || !w.track) return;
      sent = true;
      w.app.send({ type: 'canvas', id: w.uid, track: w.track, canvas: offscreen }, [offscreen]);
    };
    attach();
    return {
      wants: () => (w.track ? [{ track: w.track }] : []),
      update() {
        attach();
        const t = w.track ? w.app.state?.broadcasts.flatMap((b) => b.tracks.map((x) => [`${b.name}/${x.info.id}`, x] as const)).find(([p]) => p === w.track)?.[1] : undefined;
        w.root.classList.toggle('offline', !!w.app.state && !t?.online);
        overlay.hidden = w.prop<string>('hud', 'full') === 'off';
      },
      camera(s) {
        const hud = w.prop<string>('hud', 'full');
        w.root.classList.toggle('stale', s.fps < 0.5);
        if (hud === 'off') return;
        if (hud === 'compact') {
          overlay.textContent = `${s.codec ?? '–'} ${s.size} ${fmt(s.fps, 0)} fps  glass≈${s.glassToGlass.avg === null ? '–' : s.glassToGlass.avg.toFixed(0)} ms`;
          return;
        }
        // Capture times are the owner's receive time without the encoder's SEI.
        const sei = s.captureFromSei ? '' : ' (no SEI: capture = publisher time)';
        overlay.textContent = [
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
          `  glass→glass≈  ${win(s.glassToGlass)}  ±${fmt(s.syncErrorMs, 1)} sync`,
        ].join('\n');
      },
      destroy: () => w.app.send({ type: 'uncanvas', id: w.uid }),
    };
  },
};

// --- latency per hop ---------------------------------------------------------

const HOPS: [keyof CameraStats, string][] = [
  ['captureToPublisher', 'capture → publisher'],
  ['publisherToRelay', 'publisher → relay'],
  ['relayToBrowser', 'relay → here'],
  ['decode', 'decode'],
  ['drawToDisplay', 'draw → display'],
];

const latency: Def = {
  type: 'latency',
  label: 'Latency per hop',
  size: [3, 4],
  binds: ['video'],
  props: [{ key: 'scale_ms', label: 'Full scale (ms)', kind: 'number', default: 100 }],
  cost: { decode: false, redraw: '2 Hz', framePath: false },
  create(w) {
    const rows = HOPS.map(([, label]) => {
      const row = el('div', 'hop');
      const bar = el('div', 'bar');
      const fill = el('div', 'fill');
      const max = el('div', 'max');
      bar.append(fill, max);
      const value = el('span', 'v');
      row.append(el('span', 'k', label), bar, value);
      w.body.append(row);
      return { fill, max, value };
    });
    const total = el('div', 'total', 'glass → glass ≈ –');
    w.body.classList.add('latency');
    w.body.append(total);
    return {
      wants: () => (w.track ? [{ track: w.track }] : []),
      camera(s) {
        const scale = Math.max(1, w.prop('scale_ms', 100));
        HOPS.forEach(([k], i) => {
          const v = s[k] as Window;
          const r = rows[i]!;
          r.fill.style.width = `${Math.min(100, ((v.avg ?? 0) / scale) * 100)}%`;
          r.max.style.left = `${Math.min(100, ((v.max ?? 0) / scale) * 100)}%`;
          r.value.textContent = win(v);
        });
        total.textContent = `glass → glass ≈ ${win(s.glassToGlass)} ms ±${fmt(s.syncErrorMs, 1)}${s.captureFromSei ? '' : ' (no SEI)'}`;
      },
    };
  },
};

// --- number --------------------------------------------------------------------

const number: Def = {
  type: 'number',
  label: 'Number',
  size: [2, 2],
  binds: ['data', 'field'],
  props: [
    { key: 'label', label: 'Label', kind: 'text' },
    { key: 'unit', label: 'Unit', kind: 'text', help: "Default: the track's meta.units" },
    { key: 'digits', label: 'Decimals', kind: 'number', default: 2 },
    { key: 'max_rate_hz', label: 'Max rate (Hz)', kind: 'number', default: 30, help: 'The relay thins the track to this; 0 = every message' },
  ],
  cost: { decode: false, redraw: 'on change, at most once per frame', framePath: false },
  create(w) {
    const value = el('div', 'big', '–');
    const unit = el('span', 'unit');
    w.body.classList.add('number');
    w.body.append(value, unit);
    let latest: unknown;
    let at = 0;
    return {
      wants: () => (w.track ? [{ track: w.track, ...optRate(w.prop('max_rate_hz', 30)) }] : []),
      data(d) {
        latest = getField(d.value, w.spec.bind?.path ?? '');
        at = performance.now();
        w.dirty();
      },
      draw() {
        const n = num(latest);
        value.textContent = n !== null ? n.toFixed(w.prop('digits', 2)) : latest === undefined ? '–' : String(latest);
        unit.textContent = w.prop('unit', '') || unitOf(w.track ? w.app.catalog.track(w.track) : undefined, w.spec.bind?.path ?? '');
        w.root.classList.remove('stale');
      },
      slow(now) {
        w.root.classList.toggle('stale', now - at > 2000);
      },
    };
  },
};

// --- plot -------------------------------------------------------------------------

const PLOT_CAPACITY = 8192;

const plot: Def = {
  type: 'plot',
  label: 'Plot',
  size: [4, 4],
  binds: ['data', 'field'],
  props: [
    { key: 'label', label: 'Label', kind: 'text' },
    { key: 'window_s', label: 'Window (s)', kind: 'number', default: 10 },
    { key: 'min', label: 'Min (empty = auto)', kind: 'number' },
    { key: 'max', label: 'Max (empty = auto)', kind: 'number' },
    { key: 'max_rate_hz', label: 'Max rate (Hz)', kind: 'number', default: 0, help: '0 = every sample' },
  ],
  cost: { decode: false, redraw: 'once per display frame while data arrives', framePath: false },
  create(w) {
    const canvas = el('canvas');
    w.body.classList.add('plot');
    w.body.append(canvas);
    const ctx = canvas.getContext('2d')!;
    // A ring of (time ms, value).
    const ts = new Float64Array(PLOT_CAPACITY);
    const vs = new Float64Array(PLOT_CAPACITY);
    let head = 0;
    let len = 0;
    const resize = new ResizeObserver(() => w.dirty());
    resize.observe(w.body);
    return {
      wants: () => (w.track ? [{ track: w.track, ...optRate(w.prop('max_rate_hz', 0)) }] : []),
      data(d) {
        const v = num(getField(d.value, w.spec.bind?.path ?? ''));
        if (v === null) return;
        ts[head] = d.atUs / 1000;
        vs[head] = v;
        head = (head + 1) % PLOT_CAPACITY;
        len = Math.min(len + 1, PLOT_CAPACITY);
        w.dirty();
      },
      slow: () => w.dirty(),
      draw() {
        const dpr = devicePixelRatio || 1;
        const cw = Math.max(1, Math.round(w.body.clientWidth * dpr));
        const ch = Math.max(1, Math.round(w.body.clientHeight * dpr));
        if (canvas.width !== cw || canvas.height !== ch) {
          canvas.width = cw;
          canvas.height = ch;
        }
        ctx.clearRect(0, 0, cw, ch);
        const now = Date.now();
        const span = Math.max(0.5, w.prop('window_s', 10)) * 1000;
        let lo = Infinity;
        let hi = -Infinity;
        let first = -1;
        for (let i = 0; i < len; i += 1) {
          const j = (head - len + i + PLOT_CAPACITY) % PLOT_CAPACITY;
          if (ts[j]! < now - span) continue;
          if (first < 0) first = i;
          lo = Math.min(lo, vs[j]!);
          hi = Math.max(hi, vs[j]!);
        }
        const pmin = num(w.spec.props?.min);
        const pmax = num(w.spec.props?.max);
        if (pmin !== null) lo = pmin;
        if (pmax !== null) hi = pmax;
        if (first < 0 || !Number.isFinite(lo)) return;
        if (hi - lo < 1e-9) {
          hi += 0.5;
          lo -= 0.5;
        }
        const pad = 4 * dpr;
        const x = (t: number) => ((t - (now - span)) / span) * cw;
        const y = (v: number) => pad + (1 - (v - lo) / (hi - lo)) * (ch - 2 * pad);
        ctx.strokeStyle = '#2a313c';
        ctx.lineWidth = 1;
        ctx.beginPath();
        for (const g of [lo, (lo + hi) / 2, hi]) {
          ctx.moveTo(0, y(g));
          ctx.lineTo(cw, y(g));
        }
        ctx.stroke();
        ctx.strokeStyle = '#f5c400';
        ctx.lineWidth = 1.5 * dpr;
        ctx.beginPath();
        for (let i = first; i < len; i += 1) {
          const j = (head - len + i + PLOT_CAPACITY) % PLOT_CAPACITY;
          if (i === first) ctx.moveTo(x(ts[j]!), y(vs[j]!));
          else ctx.lineTo(x(ts[j]!), y(vs[j]!));
        }
        ctx.stroke();
        ctx.fillStyle = '#8b96a5';
        ctx.font = `${11 * dpr}px ui-monospace, monospace`;
        const last = vs[(head - 1 + PLOT_CAPACITY) % PLOT_CAPACITY]!;
        ctx.fillText(`${hi.toPrecision(4)}`, pad, pad + 10 * dpr);
        ctx.fillText(`${lo.toPrecision(4)}`, pad, ch - pad);
        ctx.fillStyle = '#d6dde6';
        const text = last.toPrecision(5);
        ctx.fillText(text, cw - ctx.measureText(text).width - pad, pad + 10 * dpr);
      },
      destroy: () => resize.disconnect(),
    };
  },
};

// --- values (every field of a data track) -------------------------------------

const values: Def = {
  type: 'values',
  label: 'Values',
  size: [3, 4],
  binds: ['data'],
  props: [{ key: 'max_rate_hz', label: 'Max rate (Hz)', kind: 'number', default: 10, help: '0 = every message' }],
  cost: { decode: false, redraw: 'on change, at most once per frame', framePath: false },
  create(w) {
    const table = el('table', 'values');
    w.body.append(table);
    const rows = new Map<string, HTMLTableCellElement>();
    let latest: unknown;
    let at = 0;
    const walk = (v: unknown, prefix: string, out: [string, unknown][]) => {
      if (out.length > 64) return;
      if (typeof v === 'object' && v !== null) for (const [k, x] of Object.entries(v)) walk(x, prefix ? `${prefix}.${k}` : k, out);
      else out.push([prefix || '(value)', v]);
    };
    return {
      wants: () => (w.track ? [{ track: w.track, ...optRate(w.prop('max_rate_hz', 10)) }] : []),
      data(d) {
        latest = d.value;
        at = performance.now();
        w.dirty();
      },
      draw() {
        if (latest === undefined) return;
        const out: [string, unknown][] = [];
        walk(latest, '', out);
        const info = w.track ? w.app.catalog.track(w.track) : undefined;
        for (const [k, v] of out) {
          let cell = rows.get(k);
          if (!cell) {
            const tr = el('tr');
            cell = el('td', 'v');
            tr.append(el('td', 'k', k), cell);
            table.append(tr);
            rows.set(k, cell);
          }
          const n = num(v);
          const unit = unitOf(info, k);
          cell.textContent = `${n !== null ? (Number.isInteger(n) ? String(n) : n.toFixed(3)) : JSON.stringify(v)}${unit ? ` ${unit}` : ''}`;
        }
        w.root.classList.remove('stale');
      },
      slow(now) {
        w.root.classList.toggle('stale', at > 0 && now - at > 2000);
      },
    };
  },
};

// --- button, toggle, slider --------------------------------------------------------

/** The field a channel's `schema` names when it has exactly one of `type`. */
function schemaField(w: Host, type: string, fallback: string): string {
  const schema = w.channel ? w.app.catalog.channel(w.channel)?.schema : undefined;
  if (typeof schema === 'object' && schema) {
    const keys = Object.entries(schema).filter(([, t]) => t === type);
    if (keys.length === 1) return keys[0]![0];
  }
  return fallback;
}

/** Ack line and enabled state shared by the widgets that send. */
function sender(w: Host, controls: () => (HTMLButtonElement | HTMLInputElement)[]) {
  const status = el('div', 'ack');
  let last: AckItem | null = null;
  const update = () => {
    const why = cannotWrite(w);
    for (const c of controls()) {
      c.disabled = why !== null;
      c.title = why ?? '';
    }
    status.textContent = why ?? ackText(last);
    status.className = `ack ${why ? 'muted' : last && last.status !== 'ok' ? 'bad' : ''}`;
  };
  return {
    status,
    update,
    ack(a: AckItem) {
      if (a.channel !== w.channel) return;
      last = a;
      update();
    },
    send(body: unknown) {
      if (!w.channel || cannotWrite(w)) return;
      w.app.send({ type: 'message', channel: w.channel, body });
    },
  };
}

const button: Def = {
  type: 'button',
  label: 'Button',
  size: [2, 2],
  binds: ['channel'],
  props: [
    { key: 'label', label: 'Label', kind: 'text' },
    { key: 'message', label: 'Message (JSON)', kind: 'json', default: {} },
  ],
  cost: { decode: false, redraw: 'on click', framePath: false },
  writes: true,
  create(w) {
    const b = el('button', 'wide', w.prop('label', '') || w.channel?.split('/')[1] || 'Send');
    const s = sender(w, () => [b]);
    b.onclick = () => s.send(w.prop('message', {}));
    w.body.append(b, s.status);
    return { update: s.update, ack: s.ack };
  },
};

const toggle: Def = {
  type: 'toggle',
  label: 'Toggle',
  size: [2, 2],
  binds: ['channel'],
  optional: ['data', 'field'],
  props: [
    { key: 'label', label: 'Label', kind: 'text' },
    { key: 'field', label: 'Field', kind: 'text', help: "Default: the channel schema's one bool field, else `on`" },
  ],
  cost: { decode: false, redraw: 'on change', framePath: false },
  writes: true,
  create(w) {
    const b = el('button', 'wide toggle');
    const s = sender(w, () => [b]);
    let on = false;
    const show = () => {
      b.textContent = `${w.prop('label', '') || w.channel?.split('/')[1] || 'toggle'}: ${on ? 'ON' : 'off'}`;
      b.classList.toggle('on', on);
    };
    b.onclick = () => {
      on = !on;
      s.send({ [w.prop('field', '') || schemaField(w, 'bool', 'on')]: on });
      show();
    };
    show();
    w.body.append(b, s.status);
    return {
      // With a track bound too, the device's own state shows.
      wants: () => (w.track ? [{ track: w.track, max_rate_hz: 10 }] : []),
      data(d) {
        const v = getField(d.value, w.spec.bind?.path ?? '');
        if (typeof v === 'boolean') {
          on = v;
          w.dirty();
        }
      },
      draw: show,
      update: s.update,
      ack: s.ack,
    };
  },
};

const slider: Def = {
  type: 'slider',
  label: 'Slider',
  size: [3, 2],
  binds: ['channel'],
  props: [
    { key: 'label', label: 'Label', kind: 'text' },
    { key: 'field', label: 'Field', kind: 'text', help: "Default: the channel schema's one number field, else `value`" },
    { key: 'min', label: 'Min', kind: 'number', default: 0 },
    { key: 'max', label: 'Max', kind: 'number', default: 1 },
    { key: 'step', label: 'Step', kind: 'number', default: 0.01 },
    { key: 'rate_hz', label: 'Send rate while dragging (Hz)', kind: 'number', default: 10 },
  ],
  cost: { decode: false, redraw: 'on input', framePath: false },
  writes: true,
  create(w) {
    const input = el('input');
    input.type = 'range';
    input.min = String(w.prop('min', 0));
    input.max = String(w.prop('max', 1));
    input.step = String(w.prop('step', 0.01));
    input.value = input.min;
    const value = el('span', 'unit', input.value);
    const s = sender(w, () => [input]);
    const field = () => w.prop('field', '') || schemaField(w, 'number', 'value');
    let lastSent = 0;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const push = () => {
      lastSent = performance.now();
      timer = null;
      s.send({ [field()]: Number(input.value) });
    };
    // While dragging: at most rate_hz, the newest value last.
    input.oninput = () => {
      value.textContent = input.value;
      const gap = 1000 / Math.max(1, w.prop('rate_hz', 10));
      const wait = gap - (performance.now() - lastSent);
      if (wait <= 0) push();
      else timer ??= setTimeout(push, wait);
    };
    const row = el('div', 'row');
    row.append(el('span', '', w.prop('label', '') || w.channel?.split('/')[1] || ''), input, value);
    w.body.append(row, s.status);
    return { update: s.update, ack: s.ack, destroy: () => timer && clearTimeout(timer) };
  },
};

// --- teleop: drive, lock, status -----------------------------------------------

function lockHolder(app: App, lock: Path | null): string | null | undefined {
  if (!lock) return undefined;
  const [b, id] = lock.split('/');
  return app.state?.broadcasts.find((x) => x.name === b)?.locks.find((l) => l.info.id === id)?.holder;
}

const drive: Def = {
  type: 'drive',
  label: 'Drive (keyboard / gamepad)',
  size: [3, 4],
  binds: ['channel', 'lock'],
  props: [
    { key: 'rate_hz', label: 'Rate (Hz)', kind: 'number', default: 20 },
    { key: 'input', label: 'Input', kind: 'select', options: ['both', 'keyboard', 'gamepad'], default: 'both' },
    { key: 'command', label: 'Command', kind: 'text', default: 'drive' },
  ],
  cost: { decode: false, redraw: 'at its send rate', framePath: false },
  writes: true,
  create(w) {
    const { app } = w;
    const state = el('div', 'drive-state');
    const cmd = el('span', 'pill', 'throttle 0 steer 0');
    const stream = el('input');
    stream.type = 'checkbox';
    const streamLabel = el('label');
    streamLabel.append(stream, ` stream ${w.prop('rate_hz', 20)} Hz`);
    const once = el('button', 'small', 'send one');
    const row = el('div', 'row');
    row.append(streamLabel, once, cmd);
    const help = el('div', 'muted');
    help.innerHTML = 'hold <kbd>W</kbd><kbd>A</kbd><kbd>S</kbd><kbd>D</kbd> or a gamepad stick · <kbd>Space</kbd> = STOP';
    const stats = el('pre');
    w.body.classList.add('drive');
    w.body.append(state, row, help, stats);
    const mine = () => !!app.session && lockHolder(app, w.lock) === app.session;
    let shown = '';
    const sendDrive = (args: { throttle: number; steer: number }) => {
      if (!w.channel || !mine()) return;
      app.send({ type: 'message', channel: w.channel, body: { command: w.prop('command', 'drive'), args } });
      const desc = `throttle ${+args.throttle.toFixed(2)} steer ${+args.steer.toFixed(2)}`;
      if (desc !== shown) cmd.textContent = shown = desc;
    };
    // Sends while input is held or "stream" is on. Stopping lets the robot
    // trip its dead-man after the lock's message_stale_ms, by design. (The
    // relay drops drive messages from sessions that do not hold the lock.)
    const timer = setInterval(() => {
      const input = app.driveInput(w.prop('input', 'both'));
      if (input || stream.checked) sendDrive(input ?? { throttle: 0, steer: 0 });
    }, 1000 / Math.max(1, Math.min(100, w.prop('rate_hz', 20))));
    once.onclick = () => sendDrive(app.driveInput(w.prop('input', 'both')) ?? { throttle: 0, steer: 0 });
    const update = () => {
      const why = !app.operator ? 'viewers cannot drive' : w.channel && !app.catalog.can('write', w.channel) ? `no permission to write ${w.channel}` : null;
      const holder = lockHolder(app, w.lock);
      state.textContent = why ?? (holder === undefined ? `no lock ${w.lock ?? ''}` : mine() ? `driving ${w.lock}` : holder ? `${w.lock} held by ${app.who(holder)}: claim it to drive` : `${w.lock} is free: claim it to drive`);
      state.className = `drive-state ${mine() ? 'ok' : ''}`;
      stream.disabled = once.disabled = !!why;
    };
    return {
      update,
      slow() {
        const t = app.teleop;
        stats.textContent = t
          ? [
              `ack rtt ${win(t.ackRtt)} ms   heartbeat rtt ${win(t.heartbeatRtt)} ms`,
              `acks ${Object.entries(t.acks).map(([k, v]) => `${k}=${v}`).join(' ') || '–'}`,
              t.lastAckDetail ? `last: ${t.lastAckDetail}` : '',
            ].join('\n')
          : '';
      },
      stopped() {
        stream.checked = false;
      },
      destroy: () => clearInterval(timer),
    };
  },
};

const lock: Def = {
  type: 'lock',
  label: 'Lock control',
  size: [3, 3],
  binds: ['lock'],
  props: [],
  cost: { decode: false, redraw: 'on change', framePath: false },
  writes: true,
  create(w) {
    const { app } = w;
    const status = el('div', 'pill', '–');
    const claim = el('button', '', 'Claim');
    claim.title = 'Claiming while holding claims anew: that re-arms a robot after a remote stop';
    const force = el('button', '', 'Force');
    const release = el('button', '', 'Release');
    const row = el('div', 'row');
    row.append(claim, force, release);
    w.body.append(status, row);
    claim.onclick = () => w.lock && app.lock(w.lock, 'claim');
    force.onclick = () => w.lock && app.lock(w.lock, 'force');
    release.onclick = () => w.lock && app.lock(w.lock, 'release');
    return {
      update() {
        const holder = lockHolder(app, w.lock);
        const mine = !!app.session && holder === app.session;
        status.textContent = holder === undefined ? `no lock ${w.lock ?? ''}` : holder === null ? `${w.lock}: free` : mine ? `you hold ${w.lock}` : `${w.lock}: ${app.who(holder)}`;
        status.className = `pill ${mine ? 'ok' : holder ? 'warn' : ''}`;
        const can = (verb: 'lock' | 'force') => app.operator && !!w.lock && holder !== undefined && app.catalog.can(verb, w.lock);
        claim.disabled = !can('lock');
        force.disabled = !can('force') || mine;
        release.disabled = !mine;
      },
    };
  },
};

const status: Def = {
  type: 'status',
  label: 'Robot status',
  size: [3, 4],
  binds: ['broadcast'],
  props: [],
  cost: { decode: false, redraw: 'on change', framePath: false },
  create(w) {
    const { app } = w;
    const badge = el('div', 'deadman', 'dead-man: –');
    const detail = el('pre');
    w.body.append(badge, detail);
    let last: RobotStatus | undefined;
    const show = () => {
      const b = app.state?.broadcasts.find((x) => x.name === w.broadcast);
      const t = b?.owner ? app.statuses.get(b.name)?.teleop : undefined;
      if (!t) {
        badge.textContent = b?.owner ? 'dead-man: –' : `${w.broadcast ?? '–'} offline`;
        badge.className = `deadman ${b?.owner ? '' : 'tripped'}`;
      } else {
        badge.textContent = t.tripped ? `SAFE-STOPPED: ${t.reason}` : `ARMED · ${app.who(t.holder)}`;
        badge.className = `deadman ${t.tripped ? 'tripped' : 'armed'}`;
      }
      detail.textContent = [
        b ? `${b.name} ${b.owner ? `online · rtt ${fmt(b.rtt_ms, 1)} ms · clock ${fmt(b.clock_offset_us === null ? null : b.clock_offset_us / 1000, 1)} ms` : 'offline'}` : `no broadcast ${w.broadcast ?? ''}`,
        t ? `commands ${t.commands}  last seq ${t.last_seq}  remote stops ${t.stops}` : '',
        ...(last?.cameras ?? []).map((c) => `${c.id}: ${c.codec ?? '–'} ${c.frames} frames`),
      ].join('\n');
    };
    return {
      // `.status` is always subscribed; this only reads it.
      data(d) {
        last = d.value as RobotStatus;
        w.dirty();
      },
      wants: () => (w.broadcast ? [{ track: `${w.broadcast}/.status` }] : []),
      draw: show,
      update: show,
    };
  },
};

// --- log --------------------------------------------------------------------------

const log: Def = {
  type: 'log',
  label: 'Event log',
  size: [12, 4],
  binds: [],
  props: [],
  cost: { decode: false, redraw: 'on new lines', framePath: false },
  create(w) {
    const pre = el('pre', 'log');
    w.body.append(pre);
    return {
      update() {
        pre.textContent = w.app.logLines.join('\n');
      },
    };
  },
};

export const DEFS: Def[] = [video, latency, number, plot, values, button, toggle, slider, drive, lock, status, log];
export const defOf = (type: string) => DEFS.find((d) => d.type === type);
