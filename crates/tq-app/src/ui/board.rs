//! The board: the layout's widgets on its grid (docs/layout-1.md), as the
//! web console renders them. Each widget is a cached view, so a change
//! re-renders only the widgets it concerns:
//! - a decoded picture, only its video widgets' [`Video`] views;
//! - data track messages, only the widgets bound to that track (at most
//!   once per window frame: data wake-ups are coalesced and GPUI renders a
//!   notified view once per frame);
//! - acks, only the widgets that send;
//! - everything else (room state, the 2 Hz stats), through the model.
//!
//! Columns share the width; rows are `row_height` high. Positions are
//! fractions of the width, so the board needs no measuring.
//!
//! The board itself is not cached, and a video widget is not a view of its
//! own: the board places the tile's [`Video`] and [`Hud`] views directly.
//! GPUI re-renders every cached view inside a cached view that re-renders,
//! so the path from the root to a picture stays uncached.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use gpui_kit::{
    canvas,
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        slider::{Slider, SliderEvent, SliderState},
        switch::Switch,
        tag::Tag,
        v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    },
    div, point,
    prelude::*,
    px, relative, AnyView, App, Context, Entity, PathBuilder, SharedString, StyleRefinement, Window,
};
use tq_wire::{AckStatus, Path, Subscription, Verb};

use super::{
    model::{time_of_day, Model},
    tile::{Hud, Video},
    widgets::*,
};
use crate::{
    layout::{self, Kind, Layout, Widget},
    session::{DriveTarget, Shared},
};

const GAP: f32 = 8.;

/// A widget's bindings, resolved against the room.
#[derive(Clone, Default, PartialEq)]
struct Bound {
    track: Option<Path>,
    channel: Option<Path>,
    lock: Option<Path>,
    broadcast: Option<String>,
}

impl Bound {
    fn of(layout: &Layout, w: &Widget, m: &Model) -> Self {
        let room = m.state.as_deref();
        Self {
            track: layout.resolve(w, Kind::Track, room),
            channel: layout.resolve(w, Kind::Channel, room),
            lock: layout.resolve(w, Kind::Lock, room),
            broadcast: layout.broadcast_of(w, room),
        }
    }
}

enum View {
    Video(Entity<VideoWidget>),
    Other(Entity<WidgetView>),
}

struct Slot {
    spec: Widget,
    bound: Bound,
    view: View,
}

pub struct Board {
    model: Entity<Model>,
    slots: Vec<Slot>,
    /// Data seq last seen per track, and ack seq.
    seen: HashMap<Path, u64>,
    acks_seen: u64,
}

impl Board {
    pub fn new(model: Entity<Model>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |b, _, cx| b.sync(cx)).detach();
        let mut board = Self {
            model,
            slots: Vec::new(),
            seen: HashMap::new(),
            acks_seen: 0,
        };
        board.sync(cx);
        board
    }

    pub fn on_frame(&mut self, cx: &mut Context<Self>) {
        for s in &self.slots {
            if let View::Video(v) = &s.view {
                v.update(cx, |v, cx| v.on_frame(cx));
            }
        }
    }

    /// New data or acks: notify the widgets they concern.
    pub fn on_data(&mut self, cx: &mut Context<Self>) {
        let shared = self.model.read(cx).shared.clone();
        let acks = shared.ack_seq.load(Ordering::Relaxed);
        let new_acks = acks != self.acks_seen;
        self.acks_seen = acks;
        let seqs: HashMap<Path, u64> = shared
            .data
            .lock()
            .unwrap()
            .iter()
            .map(|(p, d)| (p.clone(), d.seq))
            .collect();
        let mut fresh = Vec::new();
        for (p, seq) in seqs {
            if self.seen.insert(p.clone(), seq) != Some(seq) {
                fresh.push(p);
            }
        }
        for s in &self.slots {
            let View::Other(v) = &s.view else { continue };
            let data = s.bound.track.as_ref().is_some_and(|t| fresh.contains(t));
            let ack = new_acks && s.bound.channel.is_some();
            if data || ack {
                v.update(cx, |_, cx| cx.notify());
            }
        }
    }

    /// The model changed: a new layout or room remakes the widgets whose
    /// spec or bindings changed, and the subscriptions follow.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let m = self.model.read(cx);
        let layout = m.layout.clone();
        let shared = m.shared.clone();
        let mut old: HashMap<String, Slot> = self.slots.drain(..).map(|s| (s.spec.id.clone(), s)).collect();
        let mut slots = Vec::new();
        for w in &layout.widgets {
            let bound = Bound::of(&layout, w, self.model.read(cx));
            let slot = match old.remove(&w.id) {
                Some(s) if s.spec == *w && s.bound == bound => s,
                _ => {
                    let model = self.model.clone();
                    let (spec, b) = (w.clone(), bound.clone());
                    let view = if w.kind == "video" {
                        View::Video(cx.new(|cx| VideoWidget::new(model, spec, b, cx)))
                    } else {
                        View::Other(cx.new(|cx| WidgetView::new(model, spec, b, cx)))
                    };
                    Slot {
                        spec: w.clone(),
                        bound,
                        view,
                    }
                }
            };
            slots.push(slot);
        }
        self.slots = slots;
        shared.set_wants(wants(&self.slots));
        let targets: Vec<DriveTarget> = self
            .slots
            .iter()
            .filter(|s| s.spec.kind == "drive")
            .filter_map(|s| {
                Some(DriveTarget {
                    channel: s.bound.channel.clone()?,
                    lock: s.bound.lock.clone()?,
                    rate_hz: s.spec.prop_f64("rate_hz", 20.0),
                    command: s.spec.prop_str("command", "drive").to_string(),
                })
            })
            .collect();
        *shared.drive_targets.lock().unwrap() = targets;
        cx.notify();
    }
}

/// One subscription per track the widgets are bound to: any widget that
/// wants every message wins over rate limits, else the highest rate.
fn wants(slots: &[Slot]) -> Vec<Subscription> {
    let mut merged: HashMap<Path, Option<f64>> = HashMap::new();
    for s in slots {
        let Some(track) = &s.bound.track else { continue };
        let rate = match s.spec.kind.as_str() {
            "video" | "latency" => None,
            "number" => Some(s.spec.prop_f64("max_rate_hz", 30.0)),
            "plot" => Some(s.spec.prop_f64("max_rate_hz", 0.0)),
            "values" => Some(s.spec.prop_f64("max_rate_hz", 10.0)),
            "toggle" => Some(10.0),
            _ => continue,
        }
        .filter(|r| *r > 0.0);
        merged
            .entry(track.clone())
            .and_modify(|m| {
                *m = match (*m, rate) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                }
            })
            .or_insert(rate);
    }
    let mut subs: Vec<Subscription> = merged
        .into_iter()
        .map(|(track, rate)| {
            let mut s = Subscription::new(track);
            s.max_rate_hz = rate;
            s
        })
        .collect();
    subs.sort_by(|a, b| a.track.cmp(&b.track));
    subs
}

impl Render for Board {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        let layout = m.layout.clone();
        let theme = cx.theme();
        let (cols, row) = (layout.grid.cols as f32, layout.grid.row_height as f32);
        let mut inner = div()
            .relative()
            .w_full()
            .h(px(layout.rows() as f32 * (row + GAP) + GAP));
        for s in &self.slots {
            let [x, y, w, h] = s.spec.at;
            let view = match &s.view {
                View::Video(v) => v.read(cx).tile(cx),
                View::Other(v) => AnyView::from(v.clone())
                    .cached(StyleRefinement::default().size_full())
                    .into_any_element(),
            };
            inner = inner.child(
                div()
                    .absolute()
                    .left(relative(x as f32 / cols))
                    .w(relative(w as f32 / cols))
                    .top(px(y as f32 * (row + GAP) + GAP))
                    .h(px(h as f32 * (row + GAP) - GAP))
                    .px(px(GAP / 2.))
                    .child(view),
            );
        }
        div()
            .id("board")
            .size_full()
            .overflow_y_scroll()
            .px(px(GAP / 2.))
            .bg(theme.background)
            .child(inner)
    }
}

// --- video ------------------------------------------------------------------

/// A video widget: makes the track's picture and HUD views once the
/// session has a feed for the track, and keeps the HUD's stats fresh. The
/// board draws them ([`Self::tile`]).
pub struct VideoWidget {
    model: Entity<Model>,
    bound: Bound,
    views: Option<(Entity<Video>, Entity<Hud>)>,
}

impl VideoWidget {
    fn new(model: Entity<Model>, _spec: Widget, bound: Bound, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |v, _, cx| v.sync(cx)).detach();
        let mut v = Self {
            model,
            bound,
            views: None,
        };
        v.sync(cx);
        v
    }

    fn on_frame(&mut self, cx: &mut Context<Self>) {
        if self.views.is_none() {
            self.sync(cx);
        }
        if let Some((video, _)) = &self.views {
            video.update(cx, |v, cx| v.on_frame(cx));
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.bound.track.clone() else { return };
        let m = self.model.read(cx);
        let shared = m.shared.clone();
        let stats = m
            .stats
            .iter()
            .find(|(i, _)| *i == track.to_string())
            .map(|(_, s)| s.clone());
        let sync = shared.sync_error_ms(&track.broadcast);
        if self.views.is_none() {
            let Some(feed) = shared.feeds().into_iter().find(|f| f.path == track) else {
                return;
            };
            let video = cx.new(|cx| Video::new(feed.clone(), cx));
            let hud = cx.new(|_| Hud::new(shared.clone(), feed));
            self.views = Some((video, hud));
            cx.notify();
            // The board draws the tile: it has to see the new views.
            self.model.update(cx, |_, cx| cx.notify());
        }
        if let Some((_, hud)) = &self.views {
            hud.update(cx, |h, cx| h.set_stats(stats, sync, cx));
        }
    }
}

impl VideoWidget {
    fn tile(&self, cx: &App) -> gpui_kit::AnyElement {
        let theme = cx.theme();
        let frame = div()
            .relative()
            .size_full()
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .bg(gpui_kit::black());
        match &self.views {
            Some((video, hud)) => {
                let layer = || StyleRefinement::default().absolute().size_full();
                frame
                    .child(AnyView::from(video.clone()).cached(layer()))
                    .child(AnyView::from(hud.clone()).cached(layer()))
            }
            None => frame.child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(match &self.bound.track {
                        Some(t) => format!("Waiting for {t}"),
                        None => "No video track bound".into(),
                    }),
            ),
        }
        .into_any_element()
    }
}

// --- everything else ------------------------------------------------------------

pub struct WidgetView {
    model: Entity<Model>,
    spec: Widget,
    bound: Bound,
    slider: Option<Entity<SliderState>>,
    /// The toggle's state as last sent (or reported by its track).
    on: bool,
    last_sent: Option<Instant>,
}

impl WidgetView {
    fn new(model: Entity<Model>, spec: Widget, bound: Bound, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        let slider = (spec.kind == "slider").then(|| {
            let (min, max) = (spec.prop_f64("min", 0.0) as f32, spec.prop_f64("max", 1.0) as f32);
            let state = cx.new(|_| {
                SliderState::new()
                    .min(min)
                    .max(max)
                    .step(spec.prop_f64("step", 0.01) as f32)
                    .default_value(min)
            });
            cx.subscribe(&state, |w: &mut Self, _, e: &SliderEvent, cx| {
                match e {
                    SliderEvent::Change(v) => w.slide(v.start() as f64, false, cx),
                    // Letting go always sends where it ended.
                    SliderEvent::Release(v) => w.slide(v.start() as f64, true, cx),
                }
            })
            .detach();
            state
        });
        Self {
            model,
            spec,
            bound,
            slider,
            on: false,
            last_sent: None,
        }
    }

    fn shared(&self, cx: &App) -> Arc<Shared> {
        self.model.read(cx).shared.clone()
    }

    /// Why this widget cannot send, or `None`.
    fn cannot_send(&self, cx: &App) -> Option<String> {
        let m = self.model.read(cx);
        if !m.operator() {
            return Some("Viewers cannot send".into());
        }
        let channel = self.bound.channel.as_ref()?;
        let b = m.state.as_ref().and_then(|s| s.broadcast(&channel.broadcast));
        let Some(info) = b.and_then(|b| b.channel(&channel.name)) else {
            return Some(format!("No channel {channel}"));
        };
        if !m.shared.perms().is_some_and(|p| p.allows(Verb::Write, channel)) {
            return Some(format!("No permission to write {channel}"));
        }
        if let Some(lock) = &info.info.lock {
            let lock = Path::new(&channel.broadcast, lock);
            if !m.shared.holds(&lock) {
                return Some(format!("Needs the {lock} lock"));
            }
        }
        None
    }

    fn send(&mut self, body: serde_json::Value, cx: &mut Context<Self>) {
        if self.cannot_send(cx).is_some() {
            return;
        }
        if let Some(channel) = &self.bound.channel {
            self.shared(cx).send(channel, &body);
            self.last_sent = Some(Instant::now());
        }
    }

    /// The field a channel's schema names when it has exactly one of `kind`.
    fn schema_field(&self, kind: &str, fallback: &str, cx: &App) -> String {
        let given = self.spec.prop_str("field", "");
        if !given.is_empty() {
            return given.to_string();
        }
        let m = self.model.read(cx);
        let schema = self.bound.channel.as_ref().and_then(|c| {
            let b = m.state.as_ref()?.broadcast(&c.broadcast)?;
            b.channel(&c.name)?.info.schema.clone()
        });
        let fields: Vec<String> = schema
            .as_ref()
            .and_then(|s| s.as_object())
            .map(|o| {
                o.iter()
                    .filter(|(_, t)| t.as_str() == Some(kind))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        match fields.as_slice() {
            [one] => one.clone(),
            _ => fallback.to_string(),
        }
    }

    // Sends while dragging at most `rate_hz`; the newest value goes last.
    fn slide(&mut self, value: f64, last: bool, cx: &mut Context<Self>) {
        let gap = Duration::from_secs_f64(1.0 / self.spec.prop_f64("rate_hz", 10.0).max(1.0));
        let field = self.schema_field("number", "value", cx);
        let wait = self
            .last_sent
            .map_or(Duration::ZERO, |t| gap.saturating_sub(t.elapsed()));
        if wait.is_zero() || last {
            self.send(serde_json::json!({ field: value }), cx);
            return;
        }
        let slider = self.slider.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |w, cx| {
                let Some(s) = &slider else { return };
                let v = s.read(cx).value().start() as f64;
                if w.last_sent.is_some_and(|t| t.elapsed() >= gap) {
                    w.send(serde_json::json!({ field: v }), cx);
                }
            });
        })
        .detach();
    }

    /// The newest message of the bound track.
    fn latest(&self, cx: &App) -> Option<Arc<serde_json::Value>> {
        let track = self.bound.track.as_ref()?;
        let shared = self.shared(cx);
        let data = shared.data.lock().unwrap();
        data.get(track)?.ring.back().map(|(_, v)| v.clone())
    }

    fn title(&self) -> String {
        let label = self.spec.prop_str("label", "");
        if !label.is_empty() {
            return label.to_string();
        }
        let kind = match self.spec.kind.as_str() {
            "latency" => "Latency per hop",
            "number" => "Number",
            "plot" => "Plot",
            "values" => "Values",
            "button" => "Button",
            "toggle" => "Toggle",
            "slider" => "Slider",
            "drive" => "Drive",
            "lock" => "Lock",
            "status" => "Robot status",
            "log" => "Event log",
            other => other,
        };
        let writes = matches!(
            self.spec.kind.as_str(),
            "button" | "toggle" | "slider" | "drive" | "lock"
        );
        let what = if writes {
            self.bound
                .channel
                .as_ref()
                .or(self.bound.lock.as_ref())
                .map(|p| p.to_string())
        } else if let Some(t) = &self.bound.track {
            let field = self.spec.bind().path.filter(|p| !p.is_empty());
            Some(field.map_or(t.to_string(), |f| format!("{t} · {f}")))
        } else {
            self.bound.broadcast.clone()
        };
        what.map_or(kind.to_string(), |w| format!("{kind} · {w}"))
    }

    fn ack_line(&self, cx: &App) -> Option<(String, bool)> {
        if let Some(why) = self.cannot_send(cx) {
            return Some((why, false));
        }
        let channel = self.bound.channel.as_ref()?;
        let ack = self.shared(cx).acks.lock().unwrap().get(channel).cloned()?;
        Some(match ack.status {
            AckStatus::Ok => (format!("ok · {:.0} ms", ack.rtt_ms), true),
            other => (
                format!(
                    "{other:?}{}",
                    if ack.detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", ack.detail)
                    }
                ),
                false,
            ),
        })
    }
}

impl Render for WidgetView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.spec.kind.as_str() {
            "number" => self.number(cx),
            "plot" => self.plot(cx),
            "values" => self.values(cx),
            "latency" => self.latency(cx),
            "button" | "toggle" | "slider" => self.control(cx),
            "drive" => self.drive(cx),
            "lock" => self.lock(cx),
            "status" => self.status(cx),
            "log" => self.log(cx),
            other => muted(format!("Unknown widget type \"{other}\" (made by a newer app?)"), cx).into_any_element(),
        };
        let theme = cx.theme();
        v_flex()
            .size_full()
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.title()),
            )
            .child(div().flex_1().min_h_0().p_2().child(body))
    }
}

fn muted(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// A unit from the track's `meta.units`, as tq-edge's config sets it.
fn unit_of(m: &Model, track: &Path, field: &str) -> Option<String> {
    let b = m.state.as_ref()?.broadcast(&track.broadcast)?;
    let meta = b.track(&track.name)?.info.meta.as_ref()?;
    Some(meta.get("units")?.get(field)?.as_str()?.to_string())
}

fn number_text(v: &serde_json::Value, digits: usize) -> String {
    match v {
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => format!("{f:.digits$}"),
            _ => n.to_string(),
        },
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl WidgetView {
    fn number(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let field = self.spec.bind().path.unwrap_or_default();
        let latest = self.latest(cx);
        let value = latest.as_deref().and_then(|v| layout::field(v, &field));
        let digits = self.spec.prop_f64("digits", 2.0).clamp(0.0, 9.0) as usize;
        let unit = match self.spec.prop_str("unit", "") {
            "" => self
                .bound
                .track
                .as_ref()
                .and_then(|t| unit_of(self.model.read(cx), t, &field)),
            u => Some(u.to_string()),
        };
        let theme = cx.theme();
        h_flex()
            .size_full()
            .items_baseline()
            .justify_center()
            .gap_1p5()
            .child(
                div()
                    .text_3xl()
                    .font_semibold()
                    .font_family(theme.mono_font_family.clone())
                    .child(value.map_or("–".into(), |v| number_text(v, digits))),
            )
            .children(unit.map(|u| div().text_sm().text_color(theme.muted_foreground).child(u)))
            .into_any_element()
    }

    fn plot(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let field = self.spec.bind().path.unwrap_or_default();
        let span_ms = self.spec.prop_f64("window_s", 10.0).max(0.5) * 1000.0;
        let now_ms = tq_wire::now_us() as f64 / 1000.0;
        let points: Vec<(f64, f64)> = match &self.bound.track {
            Some(t) => {
                let shared = self.shared(cx);
                let data = shared.data.lock().unwrap();
                data.get(t).map_or(Vec::new(), |d| {
                    d.ring
                        .iter()
                        .filter(|(at, _)| *at >= now_ms - span_ms)
                        .filter_map(|(at, v)| Some((*at, layout::field(v, &field)?.as_f64()?)))
                        .collect()
                })
            }
            None => Vec::new(),
        };
        let mut lo = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let mut hi = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        if let Some(v) = self.spec.props.get("min").and_then(|v| v.as_f64()) {
            lo = v;
        }
        if let Some(v) = self.spec.props.get("max").and_then(|v| v.as_f64()) {
            hi = v;
        }
        if hi - lo < 1e-9 {
            hi += 0.5;
            lo -= 0.5;
        }
        let theme = cx.theme();
        let (line, grid) = (theme.chart_1, theme.border);
        let last = points.last().map(|p| p.1);
        let labels = h_flex()
            .justify_between()
            .text_xs()
            .font_family(theme.mono_font_family.clone())
            .text_color(theme.muted_foreground)
            .child(if lo.is_finite() {
                format!("{lo:.4} – {hi:.4}")
            } else {
                "no data".into()
            })
            .child(div().text_color(theme.foreground).child(fmt(last, 3)));
        v_flex()
            .size_full()
            .gap_1()
            .child(labels)
            .child(
                div().flex_1().min_h_0().w_full().child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                            let x = |t: f64| bounds.origin.x + px(((t - (now_ms - span_ms)) / span_ms) as f32 * w);
                            let y = |v: f64| bounds.origin.y + px((1.0 - ((v - lo) / (hi - lo)) as f32) * h);
                            let mut g = PathBuilder::stroke(px(1.));
                            for v in [lo, (lo + hi) / 2.0, hi] {
                                g.move_to(point(bounds.origin.x, y(v)));
                                g.line_to(point(bounds.origin.x + px(w), y(v)));
                            }
                            if let Ok(p) = g.build() {
                                window.paint_path(p, grid);
                            }
                            if points.len() < 2 {
                                return;
                            }
                            let mut path = PathBuilder::stroke(px(1.5));
                            path.move_to(point(x(points[0].0), y(points[0].1)));
                            for &(t, v) in &points[1..] {
                                path.line_to(point(x(t), y(v)));
                            }
                            if let Ok(p) = path.build() {
                                window.paint_path(p, line);
                            }
                        },
                    )
                    .size_full(),
                ),
            )
            .into_any_element()
    }

    fn values(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(latest) = self.latest(cx) else {
            return muted("No messages yet", cx).into_any_element();
        };
        let mut rows = Vec::new();
        flatten(&latest, String::new(), &mut rows);
        let m = self.model.read(cx);
        let theme = cx.theme();
        let mut col = v_flex().gap_0p5().text_xs().font_family(theme.mono_font_family.clone());
        for (k, v) in rows {
            let unit = self.bound.track.as_ref().and_then(|t| unit_of(m, t, &k));
            let text = match unit {
                Some(u) => format!("{} {u}", number_text(&v, 3)),
                None => number_text(&v, 3),
            };
            col = col.child(
                h_flex()
                    .justify_between()
                    .child(div().text_color(theme.muted_foreground).child(k))
                    .child(text),
            );
        }
        div()
            .id("values")
            .size_full()
            .overflow_y_scroll()
            .child(col)
            .into_any_element()
    }

    fn latency(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let m = self.model.read(cx);
        let Some(track) = &self.bound.track else {
            return muted("No video track bound", cx).into_any_element();
        };
        let id = track.to_string();
        let Some(s) = m.stats.iter().find(|(i, _)| *i == id).map(|(_, s)| s) else {
            return muted(format!("Waiting for {track}"), cx).into_any_element();
        };
        let scale = self.spec.prop_f64("scale_ms", 100.0).max(1.0);
        let theme = cx.theme();
        let hops = [
            ("capture → publisher", &s.capture_to_publisher),
            ("publisher → relay", &s.publisher_to_relay),
            ("relay → app", &s.relay_to_app),
            ("decode", &s.decode),
            ("decoded → painted", &s.decoded_to_painted),
        ];
        let mut col = v_flex().gap_1().text_xs();
        for (label, w) in hops {
            let frac = (w.avg.unwrap_or(0.0) / scale).clamp(0.0, 1.0) as f32;
            let max = (w.max.unwrap_or(0.0) / scale).clamp(0.0, 1.0) as f32;
            col = col.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(px(118.)).text_color(theme.muted_foreground).child(label))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h(px(8.))
                            .rounded_full()
                            .bg(theme.background)
                            .child(div().h_full().w(relative(frac)).rounded_full().bg(theme.success))
                            .child(
                                div()
                                    .absolute()
                                    .top(px(-2.))
                                    .left(relative(max))
                                    .w(px(2.))
                                    .h(px(12.))
                                    .bg(theme.warning),
                            ),
                    )
                    .child(
                        div()
                            .w(px(70.))
                            .text_right()
                            .font_family(theme.mono_font_family.clone())
                            .child(w.to_string()),
                    ),
            );
        }
        let total = s.capture_to_render.avg;
        col.child(
            div()
                .pt_1()
                .font_semibold()
                .text_color(Level::of_ms(total, 50., 100.).color(cx))
                .child(format!("capture → render {} ms", s.capture_to_render)),
        )
        .into_any_element()
    }

    fn control(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let why = self.cannot_send(cx);
        let ack = self.ack_line(cx);
        let theme = cx.theme();
        let name = self
            .bound
            .channel
            .as_ref()
            .map_or("send".to_string(), |c| c.name.clone());
        let label = self.spec.prop_str("label", &name).to_string();
        let control = match self.spec.kind.as_str() {
            "button" => Button::new("send")
                .w_full()
                .label(label)
                .disabled(why.is_some())
                .on_click(cx.listener(|w, _, _, cx| {
                    let msg = w.spec.props.get("message").cloned().unwrap_or(serde_json::json!({}));
                    w.send(msg, cx);
                }))
                .into_any_element(),
            "toggle" => {
                // With a track bound too, the device's own state shows.
                if let Some(v) = self.latest(cx) {
                    if let Some(b) =
                        layout::field(&v, &self.spec.bind().path.unwrap_or_default()).and_then(|b| b.as_bool())
                    {
                        self.on = b;
                    }
                }
                Switch::new("toggle")
                    .checked(self.on)
                    .label(label)
                    .disabled(why.is_some())
                    .on_click(cx.listener(|w, on: &bool, _, cx| {
                        w.on = *on;
                        let field = w.schema_field("bool", "on", cx);
                        w.send(serde_json::json!({ field: *on }), cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            _ => match &self.slider {
                Some(s) => {
                    let value = s.read(cx).value().start();
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex().justify_between().text_xs().child(label).child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(format!("{value:.2}")),
                            ),
                        )
                        .child(Slider::new(s).disabled(why.is_some()))
                        .into_any_element()
                }
                None => div().into_any_element(),
            },
        };
        let theme = cx.theme();
        v_flex()
            .gap_1()
            .child(control)
            .children(ack.map(|(text, ok)| {
                div()
                    .text_xs()
                    .text_color(if ok { theme.muted_foreground } else { theme.danger })
                    .child(text)
            }))
            .into_any_element()
    }

    fn drive(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let model = self.model.clone();
        let m = self.model.read(cx);
        let shared = m.shared.clone();
        let holder = self.bound.lock.as_ref().and_then(|l| {
            let b = m.state.as_ref()?.broadcast(&l.broadcast)?;
            Some(b.lock(&l.name)?.holder.clone())
        });
        let mine = self.bound.lock.as_ref().is_some_and(|l| shared.holds(l));
        let state = match (&self.bound.lock, holder) {
            (None, _) => Tag::secondary().child("No lock bound"),
            (Some(l), None) => Tag::secondary().child(format!("No lock {l}")),
            (Some(_), Some(_)) if mine => Tag::success().child("You are driving"),
            (Some(_), Some(Some(h))) => Tag::warning().child(format!("Held by {}", short(&h))),
            (Some(_), Some(None)) => Tag::secondary().child("Free: claim it to drive"),
        };
        let drive = *shared.drive.lock().unwrap();
        let keys = m.keys.clone();
        let streaming = m.streaming;
        let rate = self.spec.prop_f64("rate_hz", 20.0);
        let key = |k: &'static str, label: &'static str, cx: &App| {
            let held = keys.contains(k);
            let t = cx.theme();
            div()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_b_2()
                .font_semibold()
                .text_xs()
                .border_color(if held { t.primary } else { t.border })
                .bg(if held { t.primary } else { t.background })
                .text_color(if held { t.primary_foreground } else { t.foreground })
                .child(label)
        };
        v_flex()
            .gap_2()
            .child(state.small())
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex().gap_1().items_center().child(key("w", "W", cx)).child(
                            h_flex()
                                .gap_1()
                                .child(key("a", "A", cx))
                                .child(key("s", "S", cx))
                                .child(key("d", "D", cx)),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(kv("throttle", format!("{:+.0}", drive.throttle), cx))
                            .child(kv("steer", format!("{:+.0}", drive.steer), cx))
                            .child(kv(
                                "sending",
                                if drive.active && mine {
                                    format!("{rate:.0} Hz")
                                } else {
                                    "idle".into()
                                },
                                cx,
                            )),
                    ),
            )
            .child(
                Switch::new("stream")
                    .checked(streaming)
                    .label(format!("Stream {rate:.0} Hz"))
                    .small()
                    // Sending needs the lock: without it the switch would
                    // do nothing.
                    .disabled(!m.operator() || !mine)
                    .on_click(move |on: &bool, _, cx| {
                        let on = *on;
                        model.update(cx, |m, cx| {
                            m.streaming = on;
                            m.update_drive(cx);
                        })
                    }),
            )
            .child(kv(
                "ack / heartbeat rtt",
                format!("{} / {} ms", m.teleop_rtt.0, m.teleop_rtt.1),
                cx,
            ))
            .into_any_element()
    }

    fn lock(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let m = self.model.read(cx);
        let shared = m.shared.clone();
        let Some(lock) = self.bound.lock.clone() else {
            return muted("No lock bound", cx).into_any_element();
        };
        let holder = m
            .state
            .as_ref()
            .and_then(|s| s.broadcast(&lock.broadcast))
            .and_then(|b| b.lock(&lock.name))
            .map(|l| l.holder.clone());
        let mine = shared.holds(&lock);
        let tag = match &holder {
            None => Tag::secondary().child(format!("No lock {lock}")),
            Some(None) => Tag::secondary().child("Free"),
            Some(Some(_)) if mine => Tag::success().child("You hold it"),
            Some(Some(h)) => Tag::warning().child(format!("Held by {}", short(h))),
        };
        let perms = shared.perms();
        let can = |verb| m.operator() && holder.is_some() && perms.as_ref().is_some_and(|p| p.allows(verb, &lock));
        let (claim, force) = (can(Verb::Lock), can(Verb::Force));
        let act = |id: &'static str, label: &'static str, f: fn(&Arc<Shared>, Path)| {
            let (shared, lock) = (shared.clone(), lock.clone());
            Button::new(id)
                .small()
                .label(label)
                .on_click(move |_, _, _| f(&shared, lock.clone()))
        };
        v_flex()
            .gap_2()
            .child(tag.small())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        act("claim", "Claim", |s, l| s.claim(l, false))
                            .primary()
                            .disabled(!claim)
                            .tooltip("Claiming while holding claims anew: that re-arms a robot after a remote stop"),
                    )
                    .child(act("force", "Force", |s, l| s.claim(l, true)).disabled(!force || mine))
                    .child(act("release", "Release", |s, l| s.release(l)).outline().disabled(!mine)),
            )
            .into_any_element()
    }

    fn status(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let m = self.model.read(cx);
        let Some(name) = &self.bound.broadcast else {
            return muted("No broadcast bound", cx).into_any_element();
        };
        let b = m.state.as_ref().and_then(|s| s.broadcast(name));
        let teleop = b.filter(|b| b.owner.is_some()).and_then(|_| m.robot_teleop(name));
        let tag = match (b, &teleop) {
            (None, _) => Tag::danger().child(format!("No broadcast {name}")),
            (Some(b), _) if b.owner.is_none() => Tag::danger().child(format!("{name} offline")),
            (_, None) => Tag::secondary().child("Dead-man –"),
            (_, Some(t)) if t.tripped => Tag::danger().child(format!("Safe-stopped · {}", t.reason)),
            (_, Some(t)) => Tag::success().child(format!("Armed · {}", t.holder.as_deref().map_or("–", short))),
        };
        let status = m.statuses.get(name);
        let n = |k: &str| {
            status
                .and_then(|s| s.get("teleop")?.get(k)?.as_u64())
                .map_or("–".to_string(), |v| v.to_string())
        };
        v_flex()
            .gap_1()
            .child(tag.small())
            .child(kv("robot rtt", format!("{} ms", fmt(b.and_then(|b| b.rtt_ms), 1)), cx))
            .child(kv("commands", n("commands"), cx))
            .child(kv("remote stops", n("stops"), cx))
            .into_any_element()
    }

    fn log(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let m = self.model.read(cx);
        let theme = cx.theme();
        let mut col = v_flex().gap_0p5().text_xs();
        for (at, line) in m.log.iter().take(100) {
            col = col.child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(time_of_day(*at)),
                    )
                    .child(line.clone()),
            );
        }
        div()
            .id("log")
            .size_full()
            .overflow_y_scroll()
            .child(col)
            .into_any_element()
    }
}

/// Leaves of a JSON value as dotted paths, at most 64.
fn flatten(v: &serde_json::Value, prefix: String, out: &mut Vec<(String, serde_json::Value)>) {
    if out.len() >= 64 {
        return;
    }
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(x, p, out);
            }
        }
        other => out.push((if prefix.is_empty() { "(value)".into() } else { prefix }, other.clone())),
    }
}
