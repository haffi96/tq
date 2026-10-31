//! Layouts (docs/layout-1.md): a grid of widgets, each bound to the room by
//! path. The native counterpart of `web/src/layout.ts`: the same documents,
//! the same presets, the same rules for resolving bare ids.

use std::collections::{BTreeMap, HashSet};

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
            let [x, _, cw, ch] = w.at;
            if cw < 1 || ch < 1 || x + cw > g.cols {
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

    /// Rows the widgets take.
    pub fn rows(&self) -> u32 {
        self.widgets.iter().map(|w| w.at[1] + w.at[3]).max().unwrap_or(0)
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
    fn fields() {
        let v = serde_json::json!({ "a": { "b": [1, 2, { "c": true }] } });
        assert_eq!(field(&v, "a.b.1"), Some(&serde_json::json!(2)));
        assert_eq!(field(&v, "a.b.2.c"), Some(&serde_json::json!(true)));
        assert_eq!(field(&v, "a.x"), None);
    }
}
