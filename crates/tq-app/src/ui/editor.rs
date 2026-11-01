//! The layout editor, in the dock where the sidebar is: add widgets, bind
//! the selected one from the room's catalog (only what this session's
//! permissions allow), set its props, pin it, see what it costs and what
//! is wrong with it. The board does the moving and resizing ([`super::board`]).
//! Every change is saved at once ([`Model::edit_layout`]); the layout moves
//! between apps as JSON through the clipboard.

use std::collections::{BTreeMap, HashMap, HashSet};

use gpui_kit::{
    component::{
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        h_flex,
        input::{Input, InputState},
        select::{Select, SelectEvent, SelectState},
        v_flex, ActiveTheme as _, IndexPath, Sizable as _, StyledExt as _,
    },
    div,
    prelude::*,
    AnyElement, App, ClipboardItem, Context, Entity, FocusHandle, SharedString, Subscription, Window,
};
use tq_wire::{Path, TrackKind, Verb};

use super::{model::Model, widgets::section};
use crate::layout::{self, def_of, Bind, BindKind, Kind, Layout, Prop, PropKind, Widget, DEFS};

/// What is wrong with each widget, or what it costs that the user should
/// know about, by widget id: the same checks as the web editor.
pub fn warnings(layout: &Layout, m: &Model) -> HashMap<String, Vec<String>> {
    let room = m.state.as_deref();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut add = |id: &str, s: String| out.entry(id.to_string()).or_default().push(s);
    let videos: Vec<(&Widget, Path)> = layout
        .widgets
        .iter()
        .filter(|w| def_of(&w.kind).is_some_and(|d| d.decode))
        .filter_map(|w| Some((w, layout.resolve(w, Kind::Track, room)?)))
        .collect();
    let decodes: HashSet<&Path> = videos.iter().map(|(_, p)| p).collect();
    for w in &layout.widgets {
        let Some(d) = def_of(&w.kind) else {
            add(&w.id, format!("unknown widget type \"{}\"", w.kind));
            continue;
        };
        for &k in d.binds {
            let name = bind_name(k);
            let path = match k {
                BindKind::Field => continue,
                BindKind::Video | BindKind::Data => layout.resolve(w, Kind::Track, room).map(|p| p.to_string()),
                BindKind::Channel => layout.resolve(w, Kind::Channel, room).map(|p| p.to_string()),
                BindKind::Lock => layout.resolve(w, Kind::Lock, room).map(|p| p.to_string()),
                BindKind::Broadcast => layout.broadcast_of(w, room),
            };
            match path {
                None => add(&w.id, format!("no {name} bound (or none in the room yet)")),
                Some(p) if room.is_some() && !exists(m, k, &p) => add(&w.id, format!("{name} {p} is not in the room")),
                Some(_) => {}
            }
        }
        if d.writes && !m.operator() {
            add(&w.id, "viewers cannot send or take locks".into());
        }
        if d.decode {
            if let Some(track) = layout.resolve(w, Kind::Track, room) {
                let same = videos.iter().filter(|(_, p)| *p == track).count();
                if same > 1 {
                    add(
                        &w.id,
                        format!("{track} is shown {same} times: one decode, {same} draws per frame"),
                    );
                }
                if decodes.len() > 4 {
                    add(
                        &w.id,
                        format!(
                            "{} videos decode at full rate: each costs a decoder and its bandwidth",
                            decodes.len()
                        ),
                    );
                }
            }
        }
        if w.kind == "plot" {
            if let Some(track) = layout.resolve(w, Kind::Track, room) {
                let rate = room
                    .and_then(|r| {
                        r.broadcast(&track.broadcast)?
                            .track(&track.name)?
                            .info
                            .data
                            .as_ref()?
                            .rate_hz
                    })
                    .unwrap_or(0.0);
                if rate > 120.0 && w.prop_f64("max_rate_hz", 0.0) <= 0.0 {
                    add(
                        &w.id,
                        format!(
                            "{track} carries {rate} Hz; the plot draws once per display frame. Set Max rate to thin it at the relay"
                        ),
                    );
                }
            }
        }
    }
    out
}

fn bind_name(k: BindKind) -> &'static str {
    match k {
        BindKind::Video => "video",
        BindKind::Data => "data",
        BindKind::Field => "field",
        BindKind::Channel => "channel",
        BindKind::Lock => "lock",
        BindKind::Broadcast => "broadcast",
    }
}

fn exists(m: &Model, k: BindKind, p: &str) -> bool {
    catalog(m, k).iter().any(|o| o.value == p)
}

/// One thing in the room a widget can be bound to.
struct Choice {
    value: String,
    label: String,
    /// This session's permissions allow it.
    allowed: bool,
}

/// The room's catalog for one kind of binding.
fn catalog(m: &Model, k: BindKind) -> Vec<Choice> {
    let Some(room) = m.state.as_deref() else {
        return Vec::new();
    };
    let perms = m.shared.perms().unwrap_or_default();
    let mut out = Vec::new();
    for b in &room.broadcasts {
        match k {
            BindKind::Video | BindKind::Data => {
                let kind = if k == BindKind::Video {
                    TrackKind::Video
                } else {
                    TrackKind::Data
                };
                for t in b.tracks.iter().filter(|t| t.info.kind == kind) {
                    let p = Path::new(&b.name, &t.info.id);
                    let allowed = perms.allows(Verb::Sub, &p);
                    let label = match (t.info.label.as_str(), allowed) {
                        ("", true) => p.to_string(),
                        (l, true) => format!("{p} · {l}"),
                        (_, false) => format!("{p} (no permission)"),
                    };
                    out.push(Choice {
                        value: p.to_string(),
                        label,
                        allowed,
                    });
                }
            }
            BindKind::Channel => {
                for c in &b.channels {
                    let p = Path::new(&b.name, &c.info.id);
                    let allowed = perms.allows(Verb::Write, &p);
                    let needs = c.info.lock.as_ref().map_or(String::new(), |l| format!(" (needs {l})"));
                    let no = if allowed { "" } else { " (no permission)" };
                    out.push(Choice {
                        value: p.to_string(),
                        label: format!("{p}{needs}{no}"),
                        allowed,
                    });
                }
            }
            BindKind::Lock => {
                for l in &b.locks {
                    let p = Path::new(&b.name, &l.info.id);
                    let allowed = perms.allows(Verb::Lock, &p);
                    out.push(Choice {
                        value: p.to_string(),
                        label: format!("{p}{}", if allowed { "" } else { " (no permission)" }),
                        allowed,
                    });
                }
            }
            BindKind::Broadcast => out.push(Choice {
                value: b.name.clone(),
                label: b.name.clone(),
                allowed: true,
            }),
            BindKind::Field => {}
        }
    }
    out
}

/// Fields seen on a data track's newest messages, with their types.
fn fields(m: &Model, track: &Path) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    let data = m.shared.data.lock().unwrap();
    if let Some(d) = data.get(track) {
        for (_, v) in d.ring.iter().rev().take(16) {
            layout::leaves(v, "", &mut out);
        }
    }
    out
}

fn slot(bind: &mut Bind, k: BindKind) -> &mut Option<String> {
    match k {
        BindKind::Video | BindKind::Data => &mut bind.track,
        BindKind::Field => &mut bind.path,
        BindKind::Channel => &mut bind.channel,
        BindKind::Lock => &mut bind.lock,
        BindKind::Broadcast => &mut bind.broadcast,
    }
}

type Picker = Entity<SelectState<Vec<SharedString>>>;

enum PropInput {
    Text(Entity<InputState>),
    Select(Picker),
}

/// The selected widget's bindings and props, as edited (applied with Apply).
struct Form {
    id: String,
    bind: Bind,
    binds: Vec<(BindKind, Picker)>,
    field: Option<Entity<InputState>>,
    props: Vec<(&'static Prop, PropInput)>,
    pinned: bool,
    errors: Vec<String>,
    _subs: Vec<Subscription>,
}

pub struct Editor {
    model: Entity<Model>,
    focus: FocusHandle,
    add: Option<Picker>,
    form: Option<Form>,
    /// The last import or export: what happened, and whether it failed.
    note: Option<(String, bool)>,
}

impl Editor {
    pub fn new(model: Entity<Model>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            focus: cx.focus_handle(),
            add: None,
            form: None,
            note: None,
        }
    }

    /// Focus is in the editor (a text field): keys are typing, not driving.
    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }

    /// Adds a widget of `kind` in the first free spot, bound to the first
    /// thing in the catalog that fits, and selects it.
    fn add_widget(&mut self, kind: &str, cx: &mut Context<Self>) {
        let Some(def) = def_of(kind) else { return };
        let m = self.model.read(cx);
        let mut bind = Bind::default();
        for &k in def.binds {
            let first = catalog(m, k)
                .into_iter()
                .filter(|c| c.allowed)
                .map(|c| c.value)
                .find(|v| match k {
                    BindKind::Video | BindKind::Data => !v.split('/').nth(1).unwrap_or("").starts_with('.'),
                    BindKind::Channel => !v.ends_with("/stop"),
                    _ => true,
                });
            if let Some(v) = first {
                *slot(&mut bind, k) = Some(v);
            }
        }
        if def.binds.contains(&BindKind::Field) {
            if let Some(track) = bind.track.as_ref().and_then(|t| t.parse::<Path>().ok()) {
                bind.path = fields(m, &track)
                    .into_iter()
                    .find(|(_, t)| *t == "number")
                    .map(|(k, _)| k);
            }
        }
        if kind == "drive" && bind.channel.is_none() {
            bind.channel = bind.lock.clone();
        }
        let layout = &m.layout;
        let id = (1..)
            .map(|n| format!("{kind}-{n}"))
            .find(|id| !layout.widgets.iter().any(|w| &w.id == id))
            .unwrap();
        let widget = Widget {
            id: id.clone(),
            kind: kind.into(),
            at: layout.free_spot(def.size.0, def.size.1),
            bind: (bind != Bind::default()).then_some(bind),
            props: BTreeMap::new(),
            pinned: false,
        };
        self.model.update(cx, |m, cx| {
            m.edit_layout(|l| l.widgets.push(widget), cx);
            m.select(Some(id), cx);
        });
    }

    fn picker(
        labels: Vec<SharedString>,
        selected: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Picker {
        cx.new(|cx| SelectState::new(labels, selected.map(IndexPath::new), window, cx))
    }

    fn build_form(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Form> {
        let m = self.model.read(cx);
        let spec = m.layout.widgets.iter().find(|w| w.id == id)?.clone();
        let def = def_of(&spec.kind);
        let bind = spec.bind();
        let mut subs = Vec::new();
        let mut binds = Vec::new();
        let mut field = None;
        let kinds: Vec<(BindKind, bool)> = def.map_or(Vec::new(), |d| {
            d.binds
                .iter()
                .map(|k| (*k, false))
                .chain(d.optional.iter().map(|k| (*k, true)))
                .collect()
        });
        let room = m.state.as_deref();
        let choices: Vec<(BindKind, bool, Vec<Choice>, Option<String>)> = kinds
            .into_iter()
            .map(|(k, optional)| {
                let resolved = match k {
                    BindKind::Video | BindKind::Data => m.layout.resolve(&spec, Kind::Track, room),
                    BindKind::Channel => m.layout.resolve(&spec, Kind::Channel, room),
                    BindKind::Lock => m.layout.resolve(&spec, Kind::Lock, room),
                    _ => None,
                };
                (k, optional, catalog(m, k), resolved.map(|p| p.to_string()))
            })
            .collect();
        let mut tmp = bind.clone();
        for (k, optional, choices, resolved) in choices {
            if k == BindKind::Field {
                let value = bind.path.clone().unwrap_or_default();
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("a field, such as battery.voltage")
                        .default_value(value)
                });
                field = Some(input);
                continue;
            }
            let current = slot(&mut tmp, k).clone();
            // A bare id matches the path it resolves to.
            let resolved = resolved.filter(|_| current.is_some()).or_else(|| current.clone());
            // `(none)` first where the binding may be empty (or is), then
            // what this session may use, then the current one if the room
            // does not have it.
            let mut values: Vec<Option<String>> = Vec::new();
            let mut labels: Vec<SharedString> = Vec::new();
            if optional || current.is_none() {
                values.push(None);
                labels.push("(none)".into());
            }
            for c in choices.iter().filter(|c| c.allowed) {
                values.push(Some(c.value.clone()));
                labels.push(c.label.clone().into());
            }
            if let Some(cur) = &resolved {
                if !values.contains(&Some(cur.clone())) {
                    values.push(Some(cur.clone()));
                    labels.push(format!("{cur} (not in the room)").into());
                }
            }
            let at = values.iter().position(|v| *v == resolved);
            let picker = Self::picker(labels.clone(), at, window, cx);
            subs.push(cx.subscribe(
                &picker,
                move |e: &mut Self, _, ev: &SelectEvent<Vec<SharedString>>, cx| {
                    let SelectEvent::Confirm(Some(label)) = ev else { return };
                    let Some(i) = labels.iter().position(|l| l == label) else {
                        return;
                    };
                    if let Some(f) = &mut e.form {
                        *slot(&mut f.bind, k) = values[i].clone();
                        cx.notify();
                    }
                },
            ));
            binds.push((k, picker));
        }
        let mut props = Vec::new();
        for p in def.map_or(&[][..], |d| d.props) {
            let cur = spec.props.get(p.key).cloned();
            let default = p
                .default
                .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok());
            let input = match p.kind {
                PropKind::Select(options) => {
                    let v = cur.or(default);
                    let at = options
                        .iter()
                        .position(|o| v.as_ref().and_then(|v| v.as_str()) == Some(o));
                    let labels = options.iter().map(|o| SharedString::from(*o)).collect();
                    PropInput::Select(Self::picker(labels, at, window, cx))
                }
                PropKind::Number | PropKind::Text | PropKind::Json => {
                    let text = match cur {
                        None => String::new(),
                        Some(serde_json::Value::String(s)) if p.kind == PropKind::Text => s,
                        Some(v) => v.to_string(),
                    };
                    let hint = p.default.unwrap_or("").trim_matches('"').to_string();
                    PropInput::Text(cx.new(|cx| InputState::new(window, cx).placeholder(hint).default_value(text)))
                }
            };
            props.push((p, input));
        }
        Some(Form {
            id: id.to_string(),
            bind,
            binds,
            field,
            props,
            pinned: spec.pinned,
            errors: Vec::new(),
            _subs: subs,
        })
    }

    /// Reads the form into the widget's spec and saves the layout.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else { return };
        let mut errors = Vec::new();
        let mut bind = form.bind.clone();
        if let Some(f) = &form.field {
            let v = f.read(cx).value().trim().to_string();
            bind.path = (!v.is_empty()).then_some(v);
        }
        let mut props = BTreeMap::new();
        for (p, input) in &form.props {
            let value = match input {
                PropInput::Select(s) => s
                    .read(cx)
                    .selected_value()
                    .map(|v| serde_json::Value::String(v.to_string())),
                PropInput::Text(t) => {
                    let text = t.read(cx).value().trim().to_string();
                    match p.kind {
                        _ if text.is_empty() => None,
                        PropKind::Number => match text.parse::<f64>() {
                            Ok(n) if n.is_finite() => Some(number(n)),
                            _ => {
                                errors.push(format!("{}: not a number", p.label));
                                None
                            }
                        },
                        PropKind::Json => match serde_json::from_str(&text) {
                            Ok(v) => Some(v),
                            Err(e) => {
                                errors.push(format!("{}: not JSON ({e})", p.label));
                                None
                            }
                        },
                        _ => Some(serde_json::Value::String(text)),
                    }
                }
            };
            if let Some(v) = value {
                props.insert(p.key.to_string(), v);
            }
        }
        form.errors = errors;
        if !form.errors.is_empty() {
            cx.notify();
            return;
        }
        let (id, pinned) = (form.id.clone(), form.pinned);
        self.model.update(cx, |m, cx| {
            m.edit_layout(
                |l| {
                    if let Some(w) = l.widgets.iter_mut().find(|w| w.id == id) {
                        // Props this build does not know stay as they were.
                        let known: HashSet<&str> =
                            def_of(&w.kind).map_or(HashSet::new(), |d| d.props.iter().map(|p| p.key).collect());
                        w.props.retain(|k, _| !known.contains(k.as_str()));
                        w.props.extend(props);
                        w.bind = (bind != Bind::default()).then_some(bind);
                        w.pinned = pinned;
                    }
                },
                cx,
            )
        });
        // Rebuilt from the saved spec on the next render.
        self.form = None;
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let json = serde_json::to_string_pretty(&*self.model.read(cx).layout).unwrap_or_default();
        cx.write_to_clipboard(ClipboardItem::new_string(json));
        self.note = Some(("Layout copied to the clipboard as JSON".into(), false));
        cx.notify();
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        let result = self.model.update(cx, |m, cx| m.import_layout(&text, cx));
        self.note = Some(match result {
            Ok(()) => ("Layout pasted from the clipboard".into(), false),
            Err(e) => (format!("The clipboard has no layout:\n{e}"), true),
        });
        self.form = None;
        cx.notify();
    }
}

/// A JSON number, whole numbers without a fraction (`20`, not `20.0`).
fn number(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        serde_json::Value::from(n as i64)
    } else {
        serde_json::Value::from(n)
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.read(cx).selected.clone();
        if self.form.as_ref().map(|f| &f.id) != selected.as_ref() {
            self.form = selected.as_deref().and_then(|id| self.build_form(id, window, cx));
        }
        let add = self
            .add
            .get_or_insert_with(|| {
                let labels = DEFS.iter().map(|d| SharedString::from(d.label)).collect();
                cx.new(|cx| SelectState::new(labels, Some(IndexPath::new(0)), window, cx))
            })
            .clone();
        let m = self.model.read(cx);
        let theme = cx.theme();
        let videos: Vec<Path> = m
            .layout
            .widgets
            .iter()
            .filter(|w| def_of(&w.kind).is_some_and(|d| d.decode))
            .filter_map(|w| m.layout.resolve(w, Kind::Track, m.state.as_deref()))
            .collect();
        let decodes = videos.iter().collect::<HashSet<_>>().len();
        let warns = warnings(&m.layout, m);
        let n: usize = warns.values().map(Vec::len).sum();
        let summary = format!(
            "{} widgets · {decodes} video decode(s), {} draw(s) per frame · {} · {}",
            m.layout.widgets.len(),
            videos.len(),
            if n > 0 {
                format!("{n} warning(s)")
            } else {
                "no warnings".into()
            },
            m.layout_place(),
        );
        let actions = h_flex()
            .gap_1()
            .flex_wrap()
            .child(
                Button::new("reset")
                    .small()
                    .outline()
                    .label("Reset to preset")
                    .tooltip("Forget the saved layout and use the one made from the room")
                    .on_click(cx.listener(|e, _, _, cx| {
                        e.form = None;
                        e.model.update(cx, |m, cx| m.reset_layout(cx));
                    })),
            )
            .child(
                Button::new("export")
                    .small()
                    .outline()
                    .label("Copy JSON")
                    .on_click(cx.listener(|e, _, _, cx| e.export(cx))),
            )
            .child(
                Button::new("import")
                    .small()
                    .outline()
                    .label("Paste JSON")
                    .tooltip("Replace the layout with one copied from the web console or a file")
                    .on_click(cx.listener(|e, _, _, cx| e.import(cx))),
            )
            .child(
                Button::new("done")
                    .small()
                    .primary()
                    .label("Done")
                    .on_click(cx.listener(|e, _, _, cx| e.model.update(cx, |m, cx| m.set_editing(false, cx)))),
            );
        let add_row = h_flex()
            .gap_1()
            .child(div().flex_1().child(Select::new(&add).small()))
            .child(
                Button::new("add")
                    .small()
                    .label("Add")
                    .on_click(cx.listener(move |e, _, _, cx| {
                        let i = add.read(cx).selected_index(cx).map_or(0, |i| i.row);
                        e.add_widget(DEFS[i.min(DEFS.len() - 1)].kind, cx);
                    })),
            );
        let note = self.note.as_ref().map(|(text, bad)| {
            div()
                .text_xs()
                .text_color(if *bad { theme.danger } else { theme.muted_foreground })
                .child(text.clone())
        });
        let form = match &self.form {
            Some(f) => self.form_view(f, warns.get(&f.id).cloned().unwrap_or_default(), cx),
            None => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Select a widget to bind and configure it. Drag a widget by its bar, resize it from its corner.")
                .into_any_element(),
        };
        let theme = cx.theme();
        div()
            .id("editor")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .gap_4()
                    .p_4()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(section("Edit layout", None, cx))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(summary))
                            .child(actions)
                            .children(note),
                    )
                    .child(v_flex().gap_2().child(section("Add widget", None, cx)).child(add_row))
                    .child(form),
            )
    }
}

impl Editor {
    fn form_view(&self, f: &Form, warns: Vec<String>, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let Some(spec) = m.layout.widgets.iter().find(|w| w.id == f.id) else {
            return div().into_any_element();
        };
        let def = def_of(&spec.kind);
        let theme = cx.theme();
        let row = |label: &str, control: AnyElement, help: &str| {
            v_flex()
                .gap_1()
                .child(div().text_xs().font_semibold().child(label.to_string()))
                .child(control)
                .when(!help.is_empty(), |c| {
                    c.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(help.to_string()),
                    )
                })
        };
        let mut col =
            v_flex()
                .gap_3()
                .child(section("Widget", None, cx))
                .child(div().text_sm().font_semibold().child(format!(
                    "{} · {}",
                    def.map_or(spec.kind.as_str(), |d| d.label),
                    spec.id
                )));
        for (k, picker) in &f.binds {
            let label = match k {
                BindKind::Video => "Video track",
                BindKind::Data => "Data track",
                BindKind::Channel => "Channel",
                BindKind::Lock => "Lock",
                _ => "Broadcast",
            };
            col = col.child(row(label, Select::new(picker).small().into_any_element(), ""));
        }
        if let Some(input) = &f.field {
            let probe = Widget {
                bind: Some(f.bind.clone()),
                ..spec.clone()
            };
            let track = m.layout.resolve(&probe, Kind::Track, m.state.as_deref());
            let seen = track.map(|t| fields(m, &t)).unwrap_or_default();
            let mut chips = h_flex().gap_1().flex_wrap();
            for (i, (k, kind)) in seen.iter().take(24).enumerate() {
                let (input, k) = (input.clone(), k.clone());
                chips = chips.child(
                    Button::new(("field", i))
                        .xsmall()
                        .ghost()
                        .label(format!("{k} ({kind})"))
                        .on_click(move |_, window, cx| {
                            input.update(cx, |s, cx| s.set_value(k.clone(), window, cx));
                        }),
                );
            }
            let help = if seen.is_empty() {
                "No messages seen yet: type a field. `a.b.0` reaches into objects and arrays"
            } else {
                "Fields seen so far on the track (click one), or type one"
            };
            col = col.child(row(
                "Field",
                v_flex()
                    .gap_1()
                    .child(Input::new(input).small())
                    .child(chips)
                    .into_any_element(),
                help,
            ));
        }
        for (p, input) in &f.props {
            let control = match input {
                PropInput::Text(t) => Input::new(t).small().into_any_element(),
                PropInput::Select(s) => Select::new(s).small().into_any_element(),
            };
            col = col.child(row(p.label, control, p.help));
        }
        col = col.child(
            Checkbox::new("pinned")
                .checked(f.pinned)
                .label("Pinned: cannot be moved, resized or removed")
                .on_click(cx.listener(|e, on: &bool, _, cx| {
                    if let Some(f) = &mut e.form {
                        f.pinned = *on;
                    }
                    cx.notify();
                })),
        );
        if let Some(d) = def {
            col = col.child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                "Cost: {}redraws {}{}.",
                if d.decode { "a video decode, " } else { "" },
                d.redraw,
                if d.decode {
                    "; on the frame path (its picture is a view of its own)"
                } else {
                    ""
                }
            )));
        }
        for w in warns {
            col = col.child(div().text_xs().text_color(theme.warning).child(format!("⚠ {w}")));
        }
        for e in &f.errors {
            col = col.child(div().text_xs().text_color(theme.danger).child(e.clone()));
        }
        col = col.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Space is STOP here too: paste text that has spaces."),
        );
        let id = f.id.clone();
        col.child(
            h_flex()
                .gap_1()
                .child(
                    Button::new("apply")
                        .small()
                        .primary()
                        .label("Apply")
                        .on_click(cx.listener(|e, _, _, cx| e.apply(cx))),
                )
                .child(
                    Button::new("revert")
                        .small()
                        .outline()
                        .label("Revert")
                        .on_click(cx.listener(|e, _, _, cx| {
                            e.form = None;
                            cx.notify();
                        })),
                )
                .when(!spec.pinned, |r| {
                    r.child(
                        Button::new("remove")
                            .small()
                            .danger()
                            .outline()
                            .label("Remove")
                            .on_click(cx.listener(move |e, _, _, cx| {
                                let id = id.clone();
                                e.model
                                    .update(cx, |m, cx| m.edit_layout(|l| l.widgets.retain(|w| w.id != id), cx));
                            })),
                    )
                }),
        )
        .into_any_element()
    }
}
