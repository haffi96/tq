// The board: renders a layout's widgets on a CSS grid, routes data to them,
// subscribes to what the visible ones are bound to, and edits the layout
// (add, remove, drag, resize, bind from the catalog, save).

import { type Layout, type WidgetSpec, freeSpot, layoutKey, parseLayout, presetLayout, settle } from './layout';
import type { Path, Subscription } from './protocol';
import { type App, type BindKind, type Def, DEFS, type Host, type Impl, defOf, el } from './widgets';
import type { AckItem, CameraStats, DataItem } from './worker';

const GAP = 8;
let uids = 0;

class Widget implements Host {
  readonly root = el('div', 'widget');
  readonly head = el('div', 'head');
  readonly title = el('span', 'title');
  readonly warn = el('span', 'warn-badge', '⚠');
  readonly tools = el('span', 'tools');
  readonly body = el('div', 'body');
  readonly uid: string;
  track: Path | null = null;
  channel: Path | null = null;
  lock: Path | null = null;
  broadcast: string | null = null;
  impl: Impl = {};
  isDirty = false;
  visible = true;
  /** What threw, once a callback has: the widget shows it and stays marked. */
  failed: string | null = null;
  readonly error = el('div', 'widget-error');
  /** Spec and resolved paths it was made with: a change remakes it. */
  key = '';

  constructor(
    readonly spec: WidgetSpec,
    readonly def: Def | undefined,
    readonly app: App,
  ) {
    this.uid = `${spec.id}#${++uids}`;
    const edit = el('span', 'edit-tools');
    this.head.append(this.title, this.warn, this.tools, edit);
    this.root.append(this.head, this.body, this.error);
    this.warn.hidden = true;
    this.error.hidden = true;
  }

  /**
   * Runs one of the implementation's callbacks. An exception (bad device
   * data, a prop out of range) marks this widget failed, in plain sight, so
   * a frozen value never passes for a live one, and never reaches the
   * other widgets or the board's loops.
   */
  run<T>(what: string, fn: () => T): T | undefined {
    try {
      return fn();
    } catch (e) {
      if (this.failed === null) console.error(`widget ${this.spec.id}: ${what} failed`, e);
      this.failed = `${what} failed: ${e instanceof Error ? e.message : String(e)}`;
      this.root.classList.add('failed');
      this.error.textContent = this.failed;
      this.error.hidden = false;
      return undefined;
    }
  }

  prop<T>(key: string, fallback: T): T {
    const v = this.spec.props?.[key];
    if (v === undefined || v === null || v === '') {
      const d = this.def?.props.find((p) => p.key === key)?.default;
      return (d ?? fallback) as T;
    }
    return v as T;
  }

  dirty() {
    this.isDirty = true;
  }
}

export class Board {
  layout: Layout;
  /** Whether the layout is the user's (stored) rather than the preset. */
  saved = false;
  /** Whether the browser refused the last write of the user's layout. */
  private storeFailed = false;
  editing = false;
  private widgets = new Map<string, Widget>();
  private routes = new Map<Path, Widget[]>();
  private wantKey = '';
  private wantsQueued = false;
  private seen = new IntersectionObserver((entries) => {
    for (const e of entries) {
      const w = [...this.widgets.values()].find((x) => x.root === e.target);
      if (w) w.visible = e.isIntersecting;
    }
    this.queueWants();
  });
  /** Main-thread time spent drawing widgets per animation frame. */
  private ui = { sum: 0, n: 0, max: 0 };
  private dialog = el('dialog', 'config');
  private summary = el('span', 'summary');

  constructor(
    private grid: HTMLElement,
    private toolbar: HTMLElement,
    private app: App,
    private storageKey: string,
  ) {
    this.layout = { version: 1, grid: { cols: 12, row_height: 40 }, widgets: [] };
    // Storage can be blocked (cookies off) or full: the board, and the
    // page's STOP with it, must start anyway.
    try {
      const stored = localStorage.getItem(storageKey);
      if (stored) {
        const { layout, errors } = parseLayout(JSON.parse(stored));
        if (layout) {
          this.layout = layout;
          this.saved = true;
        } else console.warn('stored layout ignored:', errors);
      }
    } catch (e) {
      console.warn('stored layout ignored:', e);
    }
    document.body.append(this.dialog);
    this.buildToolbar();
    document.addEventListener('visibilitychange', () => this.queueWants());
    const frame = (now: number) => {
      requestAnimationFrame(frame);
      const t0 = performance.now();
      for (const w of this.widgets.values()) {
        if (!w.isDirty) continue;
        w.isDirty = false;
        w.run('draw', () => w.impl.draw?.(now));
      }
      const dt = performance.now() - t0;
      this.ui.sum += dt;
      this.ui.n += 1;
      this.ui.max = Math.max(this.ui.max, dt);
    };
    requestAnimationFrame(frame);
    setInterval(() => {
      const now = performance.now();
      for (const w of this.widgets.values()) w.run('slow', () => w.impl.slow?.(now));
    }, 1000);
    this.render();
  }

  /** Average and worst widget draw time per frame since the last call (ms). */
  takeUiStats(): { avg: number; max: number } {
    const out = { avg: this.ui.n ? this.ui.sum / this.ui.n : 0, max: this.ui.max };
    this.ui = { sum: 0, n: 0, max: 0 };
    return out;
  }

  // --- the room and the page --------------------------------------------------

  /** The room's state (or the session) changed. */
  update() {
    if (!this.saved && this.app.state) {
      const preset = presetLayout(this.app.state, this.app.operator);
      if (layoutKey(preset) !== layoutKey(this.layout)) this.layout = preset;
    }
    this.render();
  }

  data(items: DataItem[]) {
    for (const d of items) for (const w of this.routes.get(d.track) ?? []) w.run('data', () => w.impl.data?.(d));
  }

  acks(items: AckItem[]) {
    for (const a of items) for (const w of this.widgets.values()) w.run('ack', () => w.impl.ack?.(a));
  }

  cameras(stats: Record<Path, CameraStats>) {
    for (const w of this.widgets.values()) {
      const s = w.track ? stats[w.track] : undefined;
      if (s) w.run('camera', () => w.impl.camera?.(s));
    }
  }

  /** A remote stop went out: every widget stops sending. */
  stopped() {
    for (const w of this.widgets.values()) w.run('stopped', () => w.impl.stopped?.());
  }

  /** Locks the drive widgets are bound to (a remote stop lets go of them). */
  driveLocks(): Path[] {
    return [...this.widgets.values()].filter((w) => w.spec.type === 'drive' && w.lock).map((w) => w.lock!);
  }

  /** Lines were added to the log. */
  logChanged() {
    for (const w of this.widgets.values()) if (w.spec.type === 'log') w.run('update', () => w.impl.update?.());
  }

  // --- rendering --------------------------------------------------------------------

  private render() {
    const { catalog } = this.app;
    const g = this.layout.grid;
    this.grid.style.setProperty('--cols', String(g.cols));
    this.grid.style.setProperty('--row', `${g.row_height}px`);
    const ids = new Set(this.layout.widgets.map((s) => s.id));
    for (const [id, w] of this.widgets) {
      if (ids.has(id)) continue;
      this.remove(w);
    }
    for (const spec of this.layout.widgets) {
      const b = spec.bind;
      const track = catalog.resolve('track', b, this.layout.broadcast);
      const channel = catalog.resolve('channel', b, this.layout.broadcast);
      const lock = catalog.resolve('lock', b, this.layout.broadcast);
      const broadcast = b?.broadcast ?? this.layout.broadcast ?? (track ?? channel ?? lock)?.split('/')[0] ?? null;
      const key = JSON.stringify([spec.type, spec.bind, spec.props, track, channel, lock, broadcast]);
      let w = this.widgets.get(spec.id);
      if (w && w.key !== key) {
        this.remove(w);
        w = undefined;
      }
      if (!w) {
        w = new Widget(spec, defOf(spec.type), this.app);
        Object.assign(w, { track, channel, lock, broadcast, key });
        this.mount(w);
      }
      // The layout's spec object may be new (a fresh preset) with the same content.
      (w as { spec: WidgetSpec }).spec = spec;
      this.place(w);
      w.run('update', () => w.impl.update?.());
      w.isDirty = true;
    }
    this.decorate();
    this.queueWants();
  }

  private mount(w: Widget) {
    const { spec, def } = w;
    w.title.textContent = this.titleOf(w);
    if (def) {
      w.impl = w.run('create', () => def.create(w)) ?? {};
    } else {
      w.body.append(el('div', 'muted', `unknown widget type "${spec.type}" (made by a newer app?)`));
    }
    w.root.classList.add(`w-${spec.type}`);
    this.editTools(w);
    this.grid.append(w.root);
    this.seen.observe(w.root);
    this.widgets.set(spec.id, w);
  }

  private remove(w: Widget) {
    w.run('destroy', () => w.impl.destroy?.());
    this.seen.unobserve(w.root);
    w.root.remove();
    this.widgets.delete(w.spec.id);
  }

  private titleOf(w: Widget): string {
    const label = w.spec.props?.label;
    const what = w.def?.writes ? (w.channel ?? w.lock ?? '') : w.track ? `${w.track}${w.spec.bind?.path ? ` · ${w.spec.bind.path}` : ''}` : (w.channel ?? w.lock ?? w.broadcast ?? '');
    return typeof label === 'string' && label ? label : `${w.def?.label ?? w.spec.type}${what ? ` · ${what}` : ''}`;
  }

  private place(w: Widget) {
    const [x, y, cw, ch] = w.spec.at;
    const s = w.root.style;
    s.setProperty('--x', String(x + 1));
    s.setProperty('--y', String(y + 1));
    s.setProperty('--w', String(cw));
    s.setProperty('--h', String(ch));
    w.root.classList.toggle('pinned', !!w.spec.pinned);
  }

  private queueWants() {
    if (this.wantsQueued) return;
    this.wantsQueued = true;
    queueMicrotask(() => {
      this.wantsQueued = false;
      this.pushWants();
    });
  }

  // Subscriptions: what visible widgets want, one per track. Any widget that
  // wants every message wins over rate limits; otherwise the highest rate.
  private pushWants() {
    const merged = new Map<Path, Subscription>();
    this.routes.clear();
    for (const w of this.widgets.values()) {
      for (const s of w.run('wants', () => w.impl.wants?.()) ?? []) {
        const list = this.routes.get(s.track) ?? [];
        list.push(w);
        this.routes.set(s.track, list);
        if (!w.visible || document.hidden) continue;
        const m = merged.get(s.track);
        if (!m) merged.set(s.track, { ...s });
        else if (m.max_rate_hz !== undefined) {
          if (s.max_rate_hz === undefined) delete m.max_rate_hz;
          else m.max_rate_hz = Math.max(m.max_rate_hz, s.max_rate_hz);
        }
      }
    }
    const subs = [...merged.values()].sort((a, b) => a.track.localeCompare(b.track));
    const key = JSON.stringify(subs);
    if (key === this.wantKey) return;
    this.wantKey = key;
    this.app.send({ type: 'want', subs });
  }

  // --- costs ---------------------------------------------------------------------------

  /** What each widget costs that the user should know about. */
  private warnings(): Map<string, string[]> {
    const out = new Map<string, string[]>();
    const add = (id: string, s: string) => out.set(id, [...(out.get(id) ?? []), s]);
    const ws = [...this.widgets.values()];
    const videos = ws.filter((w) => w.def?.cost.decode && w.track);
    const decodes = new Set(videos.map((w) => w.track));
    for (const w of ws) {
      const d = w.def;
      if (!d) continue;
      for (const k of d.binds) {
        if (k === 'field') continue;
        const p = k === 'video' || k === 'data' ? w.track : k === 'channel' ? w.channel : k === 'lock' ? w.lock : w.broadcast;
        if (!p) add(w.spec.id, `no ${k} bound (or none in the room yet)`);
        else if (!this.exists(k, p)) add(w.spec.id, `${k} ${p} is not in the room`);
      }
      if (d.writes && !this.app.operator) add(w.spec.id, 'viewers cannot send or take locks');
      if (d.cost.decode && w.track) {
        const same = videos.filter((v) => v.track === w.track).length;
        if (same > 1) add(w.spec.id, `${w.track} is shown ${same} times: one decode, ${same} draws per frame`);
        if (decodes.size > 4) add(w.spec.id, `${decodes.size} videos decode at full rate: each costs a decoder and its bandwidth`);
      }
      if (w.spec.type === 'plot' && w.track) {
        const rate = this.app.catalog.track(w.track)?.data?.rate_hz ?? 0;
        if (rate > 120 && !w.prop('max_rate_hz', 0)) add(w.spec.id, `${w.track} carries ${rate} Hz; the plot draws once per display frame. Set Max rate to thin it at the relay`);
      }
    }
    return out;
  }

  private exists(k: BindKind, p: Path): boolean {
    const c = this.app.catalog;
    if (k === 'video' || k === 'data') return c.tracks(k).some((t) => t.path === p);
    if (k === 'channel') return !!c.channel(p);
    if (k === 'lock') return c.locks().some((l) => l.path === p);
    return c.broadcasts().includes(p);
  }

  private decorate() {
    const warns = this.warnings();
    for (const w of this.widgets.values()) {
      const list = warns.get(w.spec.id) ?? [];
      w.warn.hidden = list.length === 0;
      w.warn.title = list.join('\n');
      w.title.textContent = this.titleOf(w);
    }
    const videos = [...this.widgets.values()].filter((w) => w.def?.cost.decode && w.track);
    const decodes = new Set(videos.map((w) => w.track)).size;
    const n = [...warns.values()].reduce((a, l) => a + l.length, 0);
    this.summary.textContent = `${this.widgets.size} widgets · ${decodes} video decode(s), ${videos.length} draw(s) per frame · ${n ? `${n} warning(s), see ⚠` : 'no warnings'} · ${!this.saved ? 'preset (not saved)' : this.storeFailed ? 'NOT saved: the browser refused it, edits are lost on reload' : 'saved in this browser'}`;
  }

  // --- editing ---------------------------------------------------------------------------

  setEditing(on: boolean) {
    this.editing = on;
    document.body.classList.toggle('editing', on);
    this.toolbar.hidden = !on;
    this.decorate();
  }

  // Saves in the browser if it can; the board follows the layout either way.
  private save() {
    this.saved = true;
    try {
      localStorage.setItem(this.storageKey, JSON.stringify(this.layout));
      this.storeFailed = false;
    } catch (e) {
      this.storeFailed = true;
      console.warn('layout not saved (kept until the page reloads):', e);
    }
    this.render();
  }

  private buildToolbar() {
    const type = el('select');
    for (const d of DEFS) type.append(new Option(d.label, d.type));
    const add = el('button', '', 'Add');
    add.onclick = () => this.addWidget(type.value);
    const reset = el('button', '', 'Reset to preset');
    reset.title = 'Forget the saved layout and use the one made from the room';
    reset.onclick = () => {
      try {
        localStorage.removeItem(this.storageKey);
      } catch (e) {
        console.warn('saved layout not removed:', e);
      }
      this.saved = false;
      this.layout = { ...this.layout, widgets: [] };
      this.update();
    };
    const exp = el('button', '', 'Export');
    exp.onclick = () => this.jsonDialog(false);
    const imp = el('button', '', 'Import');
    imp.onclick = () => this.jsonDialog(true);
    const done = el('button', '', 'Done');
    done.onclick = () => this.setEditing(false);
    this.toolbar.append(el('span', '', 'Add widget:'), type, add, reset, exp, imp, done, this.summary);
  }

  private addWidget(type: string) {
    const def = defOf(type)!;
    const c = this.app.catalog;
    const bind: Record<string, string> = {};
    // Bound to the first thing in the catalog that fits; the dialog changes it.
    for (const k of def.binds) {
      const first =
        k === 'video' || k === 'data'
          ? c.tracks(k).find((t) => !t.info.id.startsWith('.'))?.path
          : k === 'channel'
            ? c.channels().find((x) => c.can('write', x.path) && x.info.id !== 'stop')?.path
            : k === 'lock'
              ? c.locks()[0]?.path
              : k === 'broadcast'
                ? c.broadcasts()[0]
                : undefined;
      if (first) bind[k === 'video' || k === 'data' ? 'track' : k] = first;
    }
    if (def.binds.includes('field') && bind.track) {
      const f = c.fields(bind.track).find(([, x]) => x.type === 'number');
      if (f) bind.path = f[0];
    }
    if (type === 'drive' && bind.lock && !bind.channel) bind.channel = bind.lock;
    let n = 1;
    while (this.layout.widgets.some((w) => w.id === `${type}-${n}`)) n += 1;
    const spec: WidgetSpec = { id: `${type}-${n}`, type, at: freeSpot(this.layout, def.size[0], def.size[1]) };
    if (Object.keys(bind).length) spec.bind = bind;
    this.layout.widgets.push(spec);
    this.save();
    this.configure(spec.id);
  }

  private editTools(w: Widget) {
    const tools = w.head.querySelector('.edit-tools')!;
    const gear = el('button', 'small', '⚙');
    gear.title = 'Bind and configure';
    gear.onclick = () => this.configure(w.spec.id);
    const close = el('button', 'small', '×');
    close.title = 'Remove';
    close.onclick = () => {
      this.layout.widgets = this.layout.widgets.filter((s) => s.id !== w.spec.id);
      this.save();
    };
    const resize = el('div', 'resize');
    tools.append(gear, close);
    w.root.append(resize);
    w.head.onpointerdown = (e) => {
      if ((e.target as HTMLElement).closest('button')) return;
      this.drag(w, e, 'move');
    };
    resize.onpointerdown = (e) => this.drag(w, e, 'resize');
  }

  private drag(w: Widget, e: PointerEvent, mode: 'move' | 'resize') {
    if (!this.editing || w.spec.pinned) return;
    e.preventDefault();
    const target = e.currentTarget as HTMLElement;
    target.setPointerCapture(e.pointerId);
    const cols = this.layout.grid.cols;
    const rect = this.grid.getBoundingClientRect();
    const pad = parseFloat(getComputedStyle(this.grid).paddingLeft) || 0;
    const cellW = (rect.width - 2 * pad - GAP * (cols - 1)) / cols + GAP;
    const cellH = this.layout.grid.row_height + GAP;
    const start = [...w.spec.at] as WidgetSpec['at'];
    const x0 = e.clientX;
    const y0 = e.clientY;
    w.root.classList.add('dragging');
    const move = (ev: PointerEvent) => {
      const dx = Math.round((ev.clientX - x0) / cellW);
      const dy = Math.round((ev.clientY - y0) / cellH);
      const [x, y, cw, ch] = start;
      w.spec.at =
        mode === 'move'
          ? [Math.min(Math.max(0, x + dx), cols - cw), Math.max(0, y + dy), cw, ch]
          : [x, y, Math.min(Math.max(1, cw + dx), cols - x), Math.max(1, ch + dy)];
      this.place(w);
    };
    const up = () => {
      target.removeEventListener('pointermove', move);
      target.removeEventListener('pointerup', up);
      target.removeEventListener('pointercancel', up);
      w.root.classList.remove('dragging');
      settle(this.layout.widgets, w.spec.id);
      for (const x of this.widgets.values()) this.place(x);
      this.save();
    };
    target.addEventListener('pointermove', move);
    target.addEventListener('pointerup', up);
    target.addEventListener('pointercancel', up);
  }

  // The bind-and-props form of one widget.
  private configure(id: string) {
    const spec = this.layout.widgets.find((w) => w.id === id);
    if (!spec) return;
    const def = defOf(spec.type);
    const c = this.app.catalog;
    const d = this.dialog;
    d.replaceChildren();
    const form = el('form');
    form.method = 'dialog';
    form.append(el('h3', '', `${def?.label ?? spec.type} · ${spec.id}`));
    const bind = { ...spec.bind } as Record<string, string | undefined>;
    const props = { ...spec.props } as Record<string, unknown>;
    const field = (label: string, input: HTMLElement, help?: string) => {
      const row = el('label', 'field');
      row.append(el('span', '', label), input);
      if (help) row.append(el('small', 'muted', help));
      form.append(row);
    };
    const select = (options: [string, string, boolean?][], value: string | undefined, onchange: (v: string | undefined) => void, optional: boolean) => {
      const s = el('select');
      if (optional || !value) s.append(new Option('(none)', ''));
      if (value && !options.some(([v]) => v === value)) s.append(new Option(`${value} (not in the room)`, value));
      for (const [v, label, disabled] of options) {
        const o = new Option(label, v);
        o.disabled = !!disabled;
        s.append(o);
      }
      s.value = value ?? '';
      s.onchange = () => onchange(s.value || undefined);
      return s;
    };
    const fieldBox = el('div');
    const fieldPicker = () => {
      fieldBox.replaceChildren();
      const track = c.resolve('track', bind as WidgetSpec['bind'], this.layout.broadcast);
      const fields = track ? c.fields(track) : [];
      const input = el('input');
      input.value = bind.path ?? '';
      input.placeholder = fields.length ? 'pick or type a field' : 'no messages seen yet: type a field';
      const list = el('datalist');
      list.id = `fields-${spec.id}`;
      for (const [k, f] of fields) list.append(new Option(`${k} (${f.type})`, k));
      input.setAttribute('list', list.id);
      input.onchange = () => (bind.path = input.value || undefined);
      fieldBox.append(input, list);
    };
    const binds = [...(def?.binds ?? []), ...(def?.optional ?? [])];
    for (const k of binds) {
      const optional = def?.optional?.includes(k) ?? false;
      if (k === 'video' || k === 'data') {
        const opts = c.tracks(k).map((t) => [t.path, `${t.path}${t.info.label ? ` · ${t.info.label}` : ''}`, !c.can('sub', t.path)] as [string, string, boolean]);
        field(
          `${k === 'video' ? 'Video' : 'Data'} track`,
          select(opts, bind.track, (v) => ((bind.track = v), fieldPicker()), optional),
        );
      } else if (k === 'field') {
        fieldPicker();
        field('Field', fieldBox, 'Fields seen so far on the track; `a.b.0` reaches into objects and arrays');
      } else if (k === 'channel') {
        const opts = c.channels().map((x) => [x.path, `${x.path}${x.info.lock ? ` (needs ${x.info.lock})` : ''}${c.can('write', x.path) ? '' : ' (no permission)'}`, !c.can('write', x.path)] as [string, string, boolean]);
        field('Channel', select(opts, bind.channel, (v) => (bind.channel = v), optional));
      } else if (k === 'lock') {
        const opts = c.locks().map((x) => [x.path, `${x.path}${c.can('lock', x.path) ? '' : ' (no permission)'}`, !c.can('lock', x.path)] as [string, string, boolean]);
        field('Lock', select(opts, bind.lock, (v) => (bind.lock = v), optional));
      } else if (k === 'broadcast') {
        field('Broadcast', select(c.broadcasts().map((b) => [b, b]), bind.broadcast, (v) => (bind.broadcast = v), optional));
      }
    }
    const errors = el('div', 'bad');
    const readers: (() => string | null)[] = [];
    for (const p of def?.props ?? []) {
      const cur = props[p.key];
      if (p.kind === 'select') {
        field(p.label, select((p.options ?? []).map((o) => [o, o]), (cur as string) ?? (p.default as string), (v) => (props[p.key] = v), false), p.help);
      } else if (p.kind === 'bool') {
        const i = el('input');
        i.type = 'checkbox';
        i.checked = (cur ?? p.default) === true;
        i.onchange = () => (props[p.key] = i.checked);
        field(p.label, i, p.help);
      } else if (p.kind === 'json') {
        const t = el('textarea');
        t.value = JSON.stringify(cur ?? p.default ?? null);
        field(p.label, t, p.help);
        readers.push(() => {
          try {
            props[p.key] = JSON.parse(t.value);
            return null;
          } catch {
            return `${p.label}: not JSON`;
          }
        });
      } else {
        const i = el('input');
        if (p.kind === 'number') {
          i.type = 'number';
          i.step = 'any';
        }
        i.value = cur === undefined ? '' : String(cur);
        if (p.default !== undefined) i.placeholder = String(p.default);
        field(p.label, i, p.help);
        readers.push(() => {
          if (i.value === '') delete props[p.key];
          else if (p.kind === 'number') {
            const n = Number(i.value);
            if (!Number.isFinite(n)) return `${p.label}: not a number`;
            props[p.key] = n;
          } else props[p.key] = i.value;
          return null;
        });
      }
    }
    const pinned = el('input');
    pinned.type = 'checkbox';
    pinned.checked = !!spec.pinned;
    field('Pinned', pinned, 'Pinned widgets cannot be moved, resized or removed');
    if (def) {
      const cost = def.cost;
      form.append(
        el('p', 'muted', `Cost: ${cost.decode ? 'a video decode, ' : ''}redraws ${cost.redraw}${cost.framePath ? '; on the frame path (drawn in the media worker)' : ''}.`),
      );
    }
    form.append(el('p', 'muted', 'Space is STOP here too: paste text that has spaces.'), errors);
    const row = el('div', 'row');
    const apply = el('button', '', 'Apply');
    apply.value = 'apply';
    const cancel = el('button', '', 'Cancel');
    cancel.value = 'cancel';
    cancel.type = 'button';
    cancel.onclick = () => d.close();
    row.append(apply, cancel);
    form.append(row);
    form.onsubmit = (e) => {
      const problems = readers.map((r) => r()).filter((x): x is string => x !== null);
      if (problems.length) {
        e.preventDefault();
        errors.textContent = problems.join('\n');
        return;
      }
      const b = Object.fromEntries(Object.entries(bind).filter(([, v]) => v));
      if (Object.keys(b).length) spec.bind = b;
      else delete spec.bind;
      const pr = Object.fromEntries(Object.entries(props).filter(([, v]) => v !== undefined && v !== ''));
      if (Object.keys(pr).length) spec.props = pr;
      else delete spec.props;
      if (pinned.checked) spec.pinned = true;
      else delete spec.pinned;
      this.save();
    };
    d.append(form);
    d.showModal();
  }

  private jsonDialog(importing: boolean) {
    const d = this.dialog;
    d.replaceChildren();
    const form = el('form');
    form.method = 'dialog';
    form.append(el('h3', '', importing ? 'Import a layout' : 'This layout'));
    const t = el('textarea', 'json');
    t.value = importing ? '' : JSON.stringify(this.layout, null, 2);
    t.readOnly = !importing;
    const errors = el('pre', 'bad');
    const row = el('div', 'row');
    const ok = el('button', '', importing ? 'Import' : 'Close');
    row.append(ok);
    if (!importing) {
      const copy = el('button', '', 'Copy');
      copy.type = 'button';
      copy.onclick = () => void navigator.clipboard.writeText(t.value);
      row.append(copy);
    }
    form.append(t, errors, row);
    form.onsubmit = (e) => {
      if (!importing) return;
      let parsed: unknown;
      try {
        parsed = JSON.parse(t.value);
      } catch (err) {
        e.preventDefault();
        errors.textContent = String(err);
        return;
      }
      const { layout, errors: problems } = parseLayout(parsed);
      if (!layout) {
        e.preventDefault();
        errors.textContent = problems.join('\n');
        return;
      }
      this.layout = layout;
      this.save();
    };
    d.append(form);
    d.showModal();
  }
}
