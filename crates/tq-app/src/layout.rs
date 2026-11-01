//! Layouts (docs/layout-1.md): a grid of widgets, each bound to the room by
//! path. The native counterpart of `web/src/layout.ts`: the same documents,
//! the same presets, the same rules for resolving bare ids.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use tq_wire::{Path, RoomState, TrackKind};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    pub cols: u32,
    pub row_height: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    /// A field in a JSON data track's messages: `battery.voltage`, `cells.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Column, row, width, height, in cells.
    pub at: [u32; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<Bind>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub props: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The broadcast bare ids resolve against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<String>,
    pub grid: Grid,
    pub widgets: Vec<Widget>,
}

/// Where the board's layout is kept: the `--layout` file, which edits are
/// saved back to, or one file per room and preset in the settings
/// directory.
#[derive(Debug, Clone)]
pub struct Store {
    pub path: PathBuf,
    /// The user's own file (`--layout`), never removed.
    pub given: bool,
}

impl Store {
    /// The settings directory's file for a room and preset.
    pub fn for_room(dir: PathBuf, room: &str, preset: &str) -> Self {
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-_".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        Self {
            path: dir.join(format!("{}.{}.json", safe(room), safe(preset))),
            given: false,
        }
    }

    /// The stored layout; `None` when there is no file yet.
    pub fn load(&self) -> Result<Option<Layout>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(json) => Layout::parse(&json).map(Some).map_err(|e| e.join("; ")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.given => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Writes the layout whole (a temporary file renamed over the old one).
    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(layout)? + "\n";
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Forgets the stored layout; the user's own file stays.
    pub fn forget(&self) -> std::io::Result<()> {
        if self.given {
            return Ok(());
        }
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Which binding of a widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Channel,
    Lock,
}

impl Widget {
    pub fn bind(&self) -> Bind {
        self.bind.clone().unwrap_or_default()
    }

    pub fn prop_f64(&self, key: &str, default: f64) -> f64 {
        self.props.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }

    pub fn prop_str<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.props
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
    }
}

impl Layout {
    /// A layout from JSON, or every problem that stops it loading. Unknown
    /// widget types load (and render as a placeholder).
    pub fn parse(json: &str) -> Result<Self, Vec<String>> {
        let layout: Layout = serde_json::from_str(json).map_err(|e| vec![e.to_string()])?;
        let mut errors = Vec::new();
        if layout.version != VERSION {
            errors.push(format!("version must be {VERSION}, not {}", layout.version));
        }
        let g = &layout.grid;
        if !(1..=48).contains(&g.cols) || !(8..=400).contains(&g.row_height) {
            errors.push("grid needs cols 1-48 and row_height 8-400".into());
        }
        let mut ids = HashSet::new();
        for (i, w) in layout.widgets.iter().enumerate() {
            if w.id.is_empty() {
                errors.push(format!("widgets[{i}].id must not be empty"));
            } else if !ids.insert(w.id.as_str()) {
                errors.push(format!("widgets[{i}].id \"{}\" is used twice", w.id));
            }
            if w.kind.is_empty() {
                errors.push(format!("widgets[{i}].type must not be empty"));
            }
            // The web's bounds; they also keep the cell sums from overflowing.
            let [x, y, cw, ch] = w.at;
            if x >= g.cols || y > 10_000 || !(1..=g.cols).contains(&cw) || !(1..=1_000).contains(&ch) || x + cw > g.cols
            {
                errors.push(format!(
                    "widgets[{i}].at must be [col, row, width, height] inside {} columns",
                    g.cols
                ));
            }
        }
        if errors.is_empty() {
            Ok(layout)
        } else {
            Err(errors)
        }
    }

    /// The first free spot for a widget `w` cells wide and `h` high.
    pub fn free_spot(&self, w: u32, h: u32) -> [u32; 4] {
        let cols = self.grid.cols;
        let w = w.min(cols);
        for y in 0.. {
            for x in 0..=cols - w {
                let at = [x, y, w, h];
                if !self.widgets.iter().any(|o| overlaps(o.at, at)) {
                    return at;
                }
            }
        }
        unreachable!()
    }

    /// A binding as a path: `rover/front` as is; a bare `front` in the
    /// binding's broadcast, else the layout's, else the first broadcast in
    /// the room with one.
    pub fn resolve(&self, w: &Widget, kind: Kind, room: Option<&RoomState>) -> Option<Path> {
        let bind = w.bind.as_ref()?;
        let r = match kind {
            Kind::Track => bind.track.as_deref(),
            Kind::Channel => bind.channel.as_deref(),
            Kind::Lock => bind.lock.as_deref(),
        }?;
        if r.contains('/') {
            return r.parse().ok();
        }
        if let Some(b) = bind.broadcast.as_ref().or(self.broadcast.as_ref()) {
            return Some(Path::new(b, r));
        }
        let b = room?.broadcasts.iter().find(|b| match kind {
            Kind::Track => b.track(r).is_some(),
            Kind::Channel => b.channel(r).is_some(),
            Kind::Lock => b.lock(r).is_some(),
        })?;
        Some(Path::new(&b.name, r))
    }

    /// The broadcast a widget is about: its own, the layout's, or that of
    /// whatever it is bound to.
    pub fn broadcast_of(&self, w: &Widget, room: Option<&RoomState>) -> Option<String> {
        let bind = w.bind.as_ref();
        bind.and_then(|b| b.broadcast.clone())
            .or_else(|| self.broadcast.clone())
            .or_else(|| {
                [Kind::Track, Kind::Channel, Kind::Lock]
                    .into_iter()
                    .find_map(|k| self.resolve(w, k, room))
                    .map(|p| p.broadcast)
            })
    }
}

fn overlaps(a: [u32; 4], b: [u32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Moves widgets down until none overlap, `first` keeping its place (the
/// one just dropped or resized), then floats everything up as far as it
/// goes, top first. Pinned widgets never move. As `settle` in
/// web/src/layout.ts.
pub fn settle(widgets: &mut [Widget], first: Option<&str>) {
    let rank = |w: &Widget| {
        if w.pinned {
            0
        } else if Some(w.id.as_str()) == first {
            1
        } else {
            2
        }
    };
    let mut order: Vec<usize> = (0..widgets.len()).collect();
    order.sort_by_key(|&i| (rank(&widgets[i]), widgets[i].at[1], widgets[i].at[0]));
    let mut placed: Vec<[u32; 4]> = Vec::new();
    for i in order {
        if !widgets[i].pinned {
            while let Some(hit) = placed.iter().find(|p| overlaps(**p, widgets[i].at)) {
                widgets[i].at[1] = hit[1] + hit[3];
            }
        }
        placed.push(widgets[i].at);
    }
    let mut done: Vec<[u32; 4]> = widgets.iter().filter(|w| w.pinned).map(|w| w.at).collect();
    let mut free: Vec<usize> = (0..widgets.len()).filter(|&i| !widgets[i].pinned).collect();
    free.sort_by_key(|&i| (widgets[i].at[1], widgets[i].at[0]));
    for i in free {
        let at = &mut widgets[i].at;
        while at[1] > 0 && !done.iter().any(|p| overlaps(*p, [at[0], at[1] - 1, at[2], at[3]])) {
            at[1] -= 1;
        }
        done.push(*at);
    }
}

/// Where widgets go on a narrow board: one column, in layout order, each
/// as high as it is in the layout (as the web console does under 700 px).
pub fn stacked(widgets: &[Widget], cols: u32) -> Vec<[u32; 4]> {
    let mut y = 0;
    widgets
        .iter()
        .map(|w| {
            let at = [0, y, cols, w.at[3]];
            y += w.at[3];
            at
        })
        .collect()
}

// --- widget types ---------------------------------------------------------

/// What a widget can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    Video,
    Data,
    /// A field of the bound data track's messages.
    Field,
    Channel,
    Lock,
    Broadcast,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropKind {
    Number,
    Text,
    Json,
    Select(&'static [&'static str]),
}

/// A setting in a widget's `props`. `default` is JSON.
pub struct Prop {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: PropKind,
    pub default: Option<&'static str>,
    pub help: &'static str,
}

/// A widget type, as the editor offers it. The same table as `DEFS` in
/// web/src/widgets.ts (a test checks the types, labels and sizes).
pub struct Def {
    pub kind: &'static str,
    pub label: &'static str,
    /// Size a new one gets, in cells.
    pub size: (u32, u32),
    pub binds: &'static [BindKind],
    /// Bindings that may be left empty.
    pub optional: &'static [BindKind],
    pub props: &'static [Prop],
    /// Decodes video.
    pub decode: bool,
    pub redraw: &'static str,
    /// Sends messages or takes locks.
    pub writes: bool,
}

const fn prop(key: &'static str, label: &'static str, kind: PropKind) -> Prop {
    Prop {
        key,
        label,
        kind,
        default: None,
        help: "",
    }
}

const fn num(key: &'static str, label: &'static str, default: &'static str, help: &'static str) -> Prop {
    Prop {
        key,
        label,
        kind: PropKind::Number,
        default: Some(default),
        help,
    }
}

const LABEL: Prop = prop("label", "Label", PropKind::Text);

use BindKind as B;

pub const DEFS: &[Def] = &[
    Def {
        kind: "video",
        label: "Video",
        size: (6, 9),
        binds: &[B::Video],
        optional: &[],
        props: &[Prop {
            key: "hud",
            label: "Latency HUD",
            kind: PropKind::Select(&["full", "compact", "off"]),
            default: Some("\"full\""),
            help: "",
        }],
        decode: true,
        redraw: "every frame, as its own view",
        writes: false,
    },
    Def {
        kind: "latency",
        label: "Latency per hop",
        size: (3, 4),
        binds: &[B::Video],
        optional: &[],
        props: &[num("scale_ms", "Full scale (ms)", "100", "")],
        decode: false,
        redraw: "2 Hz",
        writes: false,
    },
    Def {
        kind: "number",
        label: "Number",
        size: (2, 2),
        binds: &[B::Data, B::Field],
        optional: &[],
        props: &[
            LABEL,
            Prop {
                help: "Default: the track's meta.units",
                ..prop("unit", "Unit", PropKind::Text)
            },
            num("digits", "Decimals", "2", ""),
            num(
                "max_rate_hz",
                "Max rate (Hz)",
                "30",
                "The relay thins the track to this; 0 = every message",
            ),
        ],
        decode: false,
        redraw: "on change, at most once per frame",
        writes: false,
    },
    Def {
        kind: "plot",
        label: "Plot",
        size: (4, 4),
        binds: &[B::Data, B::Field],
        optional: &[],
        props: &[
            LABEL,
            num("window_s", "Window (s)", "10", ""),
            prop("min", "Min (empty = auto)", PropKind::Number),
            prop("max", "Max (empty = auto)", PropKind::Number),
            num("max_rate_hz", "Max rate (Hz)", "0", "0 = every sample"),
        ],
        decode: false,
        redraw: "once per display frame while data arrives",
        writes: false,
    },
    Def {
        kind: "values",
        label: "Values",
        size: (3, 4),
        binds: &[B::Data],
        optional: &[],
        props: &[num("max_rate_hz", "Max rate (Hz)", "10", "0 = every message")],
        decode: false,
        redraw: "on change, at most once per frame",
        writes: false,
    },
    Def {
        kind: "button",
        label: "Button",
        size: (2, 2),
        binds: &[B::Channel],
        optional: &[],
        props: &[
            LABEL,
            Prop {
                default: Some("{}"),
                ..prop("message", "Message (JSON)", PropKind::Json)
            },
        ],
        decode: false,
        redraw: "on click",
        writes: true,
    },
    Def {
        kind: "toggle",
        label: "Toggle",
        size: (2, 2),
        binds: &[B::Channel],
        optional: &[B::Data, B::Field],
        props: &[
            LABEL,
            Prop {
                help: "Default: the channel schema's one bool field, else `on`",
                ..prop("field", "Field", PropKind::Text)
            },
        ],
        decode: false,
        redraw: "on change",
        writes: true,
    },
    Def {
        kind: "slider",
        label: "Slider",
        size: (3, 2),
        binds: &[B::Channel],
        optional: &[],
        props: &[
            LABEL,
            Prop {
                help: "Default: the channel schema's one number field, else `value`",
                ..prop("field", "Field", PropKind::Text)
            },
            num("min", "Min", "0", ""),
            num("max", "Max", "1", ""),
            num("step", "Step", "0.01", ""),
            num("rate_hz", "Send rate while dragging (Hz)", "10", ""),
        ],
        decode: false,
        redraw: "on input",
        writes: true,
    },
    Def {
        kind: "drive",
        label: "Drive (keyboard / gamepad)",
        size: (3, 4),
        binds: &[B::Channel, B::Lock],
        optional: &[],
        props: &[
            num("rate_hz", "Rate (Hz)", "20", ""),
            Prop {
                default: Some("\"both\""),
                help: "tq-app has no gamepad input yet",
                ..prop("input", "Input", PropKind::Select(&["both", "keyboard", "gamepad"]))
            },
            Prop {
                default: Some("\"drive\""),
                ..prop("command", "Command", PropKind::Text)
            },
        ],
        decode: false,
        redraw: "at its send rate",
        writes: true,
    },
    Def {
        kind: "lock",
        label: "Lock control",
        size: (3, 3),
        binds: &[B::Lock],
        optional: &[],
        props: &[],
        decode: false,
        redraw: "on change",
        writes: true,
    },
    Def {
        kind: "status",
        label: "Robot status",
        size: (3, 4),
        binds: &[B::Broadcast],
        optional: &[],
        props: &[],
        decode: false,
        redraw: "on change",
        writes: false,
    },
    Def {
        kind: "log",
        label: "Event log",
        size: (12, 4),
        binds: &[],
        optional: &[],
        props: &[],
        decode: false,
        redraw: "on new lines",
        writes: false,
    },
];

pub fn def_of(kind: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.kind == kind)
}

/// Leaves of a JSON value as dotted paths with their type (`number`,
/// `bool`, `string`), at most 256, as the web catalog finds fields.
pub fn leaves(v: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, &'static str>) {
    if out.len() >= 256 {
        return;
    }
    let kind = match v {
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                leaves(x, &join(prefix, k), out);
            }
            return;
        }
        serde_json::Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                leaves(x, &join(prefix, &i.to_string()), out);
            }
            return;
        }
        serde_json::Value::Null => return,
    };
    if !prefix.is_empty() {
        out.insert(prefix.to_string(), kind);
    }
}

fn join(prefix: &str, k: &str) -> String {
    if prefix.is_empty() {
        k.to_string()
    } else {
        format!("{prefix}.{k}")
    }
}

const DRIVE: &str = "drive";
const STOP: &str = "stop";

/// The layout the app starts with when none is given, as the web console
/// makes it (`presetLayout` in web/src/layout.ts): every broadcast's video,
/// then per broadcast the teleop controls (operators), its status and a
/// table per data track, then a latency widget and the log. Ids are
/// stable, so remaking it as the room changes keeps widgets in place.
pub fn preset(room: &RoomState, operator: bool) -> Layout {
    let mut layout = Layout {
        version: VERSION,
        name: Some(if operator { "teleop" } else { "monitor" }.into()),
        broadcast: None,
        grid: Grid {
            cols: 12,
            row_height: 40,
        },
        widgets: Vec::new(),
    };
    let add = |layout: &mut Layout,
               id: String,
               kind: &str,
               w: u32,
               h: u32,
               bind: Option<Bind>,
               props: &[(&str, serde_json::Value)]| {
        let at = layout.free_spot(w, h);
        layout.widgets.push(Widget {
            id,
            kind: kind.into(),
            at,
            bind,
            props: props.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            pinned: false,
        });
    };
    let mut first_video = None;
    for b in &room.broadcasts {
        for t in b.tracks.iter().filter(|t| t.info.kind == TrackKind::Video) {
            let p = format!("{}/{}", b.name, t.info.id);
            first_video.get_or_insert_with(|| p.clone());
            let bind = Bind {
                track: Some(p.clone()),
                ..Default::default()
            };
            add(&mut layout, format!("video:{p}"), "video", 6, 9, Some(bind), &[]);
        }
    }
    for b in &room.broadcasts {
        let teleop = b.lock(DRIVE).is_some() && b.channel(DRIVE).is_some();
        let drive = format!("{}/{DRIVE}", b.name);
        if operator && teleop {
            let lock = Bind {
                lock: Some(drive.clone()),
                ..Default::default()
            };
            add(&mut layout, format!("lock:{}", b.name), "lock", 3, 4, Some(lock), &[]);
            let bind = Bind {
                channel: Some(drive.clone()),
                lock: Some(drive.clone()),
                ..Default::default()
            };
            add(
                &mut layout,
                format!("drive:{}", b.name),
                "drive",
                3,
                4,
                Some(bind),
                &[("rate_hz", 20.into())],
            );
        }
        if teleop || b.channel(STOP).is_some() {
            let bind = Bind {
                broadcast: Some(b.name.clone()),
                ..Default::default()
            };
            add(
                &mut layout,
                format!("status:{}", b.name),
                "status",
                3,
                4,
                Some(bind),
                &[],
            );
        }
        for t in b
            .tracks
            .iter()
            .filter(|t| t.info.kind == TrackKind::Data && !t.info.id.starts_with('.'))
        {
            let p = format!("{}/{}", b.name, t.info.id);
            let bind = Bind {
                track: Some(p.clone()),
                ..Default::default()
            };
            add(&mut layout, format!("values:{p}"), "values", 3, 4, Some(bind), &[]);
        }
    }
    if let Some(p) = first_video {
        let bind = Bind {
            track: Some(p),
            ..Default::default()
        };
        add(&mut layout, "latency".into(), "latency", 3, 4, Some(bind), &[]);
    }
    add(&mut layout, "log".into(), "log", 12, 4, None, &[]);
    layout
}

/// A JSON value's field: `a.b.0` walks objects and arrays.
pub fn field<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.').try_fold(value, |v, k| match v {
        serde_json::Value::Object(m) => m.get(k),
        serde_json::Value::Array(a) => a.get(k.parse::<usize>().ok()?),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{
      "version": 1,
      "name": "teleop",
      "broadcast": "robot-01",
      "grid": { "cols": 12, "row_height": 40 },
      "widgets": [
        { "id": "front", "type": "video", "at": [0, 0, 6, 9], "bind": { "track": "front" } },
        { "id": "volts", "type": "plot", "at": [6, 0, 4, 4], "bind": { "track": "telemetry", "path": "battery_v" }, "props": { "window_s": 30 } },
        { "id": "lock", "type": "lock", "at": [6, 4, 3, 4], "bind": { "lock": "drive" }, "pinned": true },
        { "id": "drive", "type": "drive", "at": [9, 4, 3, 4], "bind": { "channel": "drive", "lock": "drive" }, "props": { "rate_hz": 20 } },
        { "id": "later", "type": "from-a-newer-app", "at": [0, 9, 2, 2] }
      ]
    }"#;

    fn room() -> RoomState {
        serde_json::from_value(serde_json::json!({
            "room_id": "r",
            "participants": [],
            "broadcasts": [{
                "index": 0, "name": "rover", "owner": "s", "owner_sid": 1, "build": null,
                "rtt_ms": null, "clock_offset_us": null, "clock_error_us": null, "quic": null,
                "tracks": [
                    { "index": 0, "online": true, "info": { "id": "front", "kind": "video", "label": "" },
                      "stats": { "frames": 0, "groups": 0, "fps": 0.0, "kbps": 0.0, "dropped": 0, "capture_to_publisher_ms": null, "codec_string": null, "subscribers": 0 } },
                    { "index": 1, "online": true, "info": { "id": "telemetry", "kind": "data", "label": "", "data": { "encoding": "json", "delivery": "latest" } },
                      "stats": { "frames": 0, "groups": 0, "fps": 0.0, "kbps": 0.0, "dropped": 0, "capture_to_publisher_ms": null, "codec_string": null, "subscribers": 0 } },
                    { "index": 2, "online": true, "info": { "id": ".status", "kind": "data", "label": "", "data": { "encoding": "json", "delivery": "latest" } },
                      "stats": { "frames": 0, "groups": 0, "fps": 0.0, "kbps": 0.0, "dropped": 0, "capture_to_publisher_ms": null, "codec_string": null, "subscribers": 0 } }
                ],
                "channels": [
                    { "index": 0, "info": { "id": "drive", "label": "", "lock": "drive", "encoding": "json" } },
                    { "index": 1, "info": { "id": "stop", "label": "", "encoding": "json" } }
                ],
                "locks": [{ "index": 0, "holder": null, "info": { "id": "drive", "label": "" } }]
            }]
        }))
        .unwrap()
    }

    #[test]
    fn parses_the_spec_example() {
        let l = Layout::parse(EXAMPLE).unwrap();
        assert_eq!(l.widgets.len(), 5);
        assert!(l.widgets[2].pinned);
        assert_eq!(l.widgets[1].prop_f64("window_s", 10.0), 30.0);
        assert_eq!(l.widgets[4].kind, "from-a-newer-app");
        // Bare ids resolve against the layout's broadcast.
        let p = l.resolve(&l.widgets[0], Kind::Track, None).unwrap();
        assert_eq!(p, Path::new("robot-01", "front"));
        // Round trip.
        let again = Layout::parse(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(again, l);
    }

    #[test]
    fn refuses_bad_layouts() {
        let bad = EXAMPLE
            .replace(r#""at": [9, 4, 3, 4]"#, r#""at": [11, 4, 3, 4]"#)
            .replace(r#""id": "lock""#, r#""id": "front""#);
        let errors = Layout::parse(&bad).unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(Layout::parse(&EXAMPLE.replace(r#""version": 1"#, r#""version": 2"#)).is_err());
        assert!(Layout::parse("[]").is_err());
        // Cells that would overflow the sums.
        for at in [
            "[4294967295, 4, 3, 4]",
            "[9, 4294967295, 3, 4]",
            "[9, 4, 3, 4294967295]",
        ] {
            let bad = EXAMPLE.replace("[9, 4, 3, 4]", at);
            assert_eq!(Layout::parse(&bad).unwrap_err().len(), 1, "{at}");
        }
    }

    #[test]
    fn bare_ids_resolve_in_the_room() {
        let mut l = Layout::parse(EXAMPLE).unwrap();
        l.broadcast = None;
        let room = room();
        assert_eq!(
            l.resolve(&l.widgets[0], Kind::Track, Some(&room)),
            Some(Path::new("rover", "front"))
        );
        assert_eq!(
            l.resolve(&l.widgets[3], Kind::Lock, Some(&room)),
            Some(Path::new("rover", "drive"))
        );
        assert_eq!(l.resolve(&l.widgets[0], Kind::Track, None), None);
    }

    #[test]
    fn preset_matches_the_web_console() {
        // The same ids and places as web/src/layout.ts makes for this room.
        let l = preset(&room(), true);
        let got: Vec<_> = l.widgets.iter().map(|w| (w.id.as_str(), w.at)).collect();
        assert_eq!(
            got,
            [
                ("video:rover/front", [0, 0, 6, 9]),
                ("lock:rover", [6, 0, 3, 4]),
                ("drive:rover", [9, 0, 3, 4]),
                ("status:rover", [6, 4, 3, 4]),
                ("values:rover/telemetry", [9, 4, 3, 4]),
                ("latency", [6, 8, 3, 4]),
                ("log", [0, 12, 12, 4]),
            ]
        );
        let viewer = preset(&room(), false);
        assert!(viewer.widgets.iter().all(|w| w.kind != "drive" && w.kind != "lock"));
    }

    #[test]
    fn settle_pushes_down_and_floats_up() {
        let w = |id: &str, at: [u32; 4], pinned| Widget {
            id: id.into(),
            kind: "number".into(),
            at,
            bind: None,
            props: BTreeMap::new(),
            pinned,
        };
        // `b` dropped onto `a`: `a` goes under it; `c` floats up to row 0.
        let mut ws = vec![
            w("a", [0, 0, 4, 2], false),
            w("b", [0, 1, 4, 2], false),
            w("c", [4, 3, 2, 2], false),
        ];
        settle(&mut ws, Some("b"));
        assert_eq!(ws[1].at, [0, 0, 4, 2]);
        assert_eq!(ws[0].at, [0, 2, 4, 2]);
        assert_eq!(ws[2].at, [4, 0, 2, 2]);
        // A pinned widget stays, even when dropped on, and nothing floats
        // up through it.
        let mut ws = vec![w("p", [0, 2, 4, 2], true), w("x", [0, 3, 4, 2], false)];
        settle(&mut ws, Some("x"));
        assert_eq!(ws[0].at, [0, 2, 4, 2]);
        assert_eq!(ws[1].at, [0, 4, 4, 2]);
        let stack = stacked(&ws, 12);
        assert_eq!(stack, [[0, 0, 12, 2], [0, 2, 12, 2]]);
    }

    #[test]
    fn defs_match_the_web_console() {
        let web = include_str!("../../../web/src/widgets.ts");
        for d in DEFS {
            let want = format!(
                "type: '{}',\n  label: '{}',\n  size: [{}, {}],",
                d.kind, d.label, d.size.0, d.size.1
            );
            assert!(web.contains(&want), "web/src/widgets.ts has no {want}");
            for p in d.props {
                assert!(web.contains(&format!("key: '{}'", p.key)), "{}.{}", d.kind, p.key);
            }
        }
        let types = web.matches("\n  type: '").count();
        assert_eq!(types, DEFS.len(), "a widget type on one side only");
    }

    #[test]
    fn leaves_of_a_message() {
        let v = serde_json::json!({ "a": 1.5, "b": { "on": true }, "c": [ "x" ], "n": null });
        let mut out = BTreeMap::new();
        leaves(&v, "", &mut out);
        let got: Vec<_> = out.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(got, [("a", "number"), ("b.on", "bool"), ("c.0", "string")]);
    }

    #[test]
    fn store_round_trip() {
        let dir = std::env::temp_dir().join(format!("tq-app-store-{}", std::process::id()));
        let store = Store::for_room(dir.clone(), "robot/01", "operator");
        assert!(store.path.ends_with("robot_01.operator.json"));
        assert_eq!(store.load(), Ok(None));
        let l = Layout::parse(EXAMPLE).unwrap();
        store.save(&l).unwrap();
        assert_eq!(store.load(), Ok(Some(l)));
        store.forget().unwrap();
        assert_eq!(store.load(), Ok(None));
        let given = Store {
            path: dir.join("missing.json"),
            given: true,
        };
        assert!(given.load().is_err(), "a given file must exist");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fields() {
        let v = serde_json::json!({ "a": { "b": [1, 2, { "c": true }] } });
        assert_eq!(field(&v, "a.b.1"), Some(&serde_json::json!(2)));
        assert_eq!(field(&v, "a.b.2.c"), Some(&serde_json::json!(true)));
        assert_eq!(field(&v, "a.x"), None);
    }
}
