# Layout spec 1

A **layout** is a JSON document: a grid of widgets, each bound to the room's
data by path. The web console and tq-app (GPUI) both render it. Keep this
file in step with `web/src/layout.ts` and `web/src/widgets.ts`, and with
`crates/tq-app/src/layout.rs` and `crates/tq-app/src/ui/board.rs`. Their
tests check that both make the same preset. `config/layout.example.json`
is a teleop layout with telemetry for `config/edge.full.example.json`.

```json
{
  "version": 1,
  "name": "teleop",
  "broadcast": "robot-01",
  "grid": { "cols": 12, "row_height": 40 },
  "widgets": [
    { "id": "front", "type": "video", "at": [0, 0, 6, 9], "bind": { "track": "front" } },
    { "id": "volts", "type": "plot", "at": [6, 0, 4, 4], "bind": { "track": "telemetry", "path": "battery_v" }, "props": { "window_s": 30 } },
    { "id": "lock", "type": "lock", "at": [6, 4, 3, 4], "bind": { "lock": "drive" }, "pinned": true },
    { "id": "drive", "type": "drive", "at": [9, 4, 3, 4], "bind": { "channel": "drive", "lock": "drive" }, "props": { "rate_hz": 20 } }
  ]
}
```

## Document

| Field | |
| --- | --- |
| `version` | `1`. |
| `name` | Optional. |
| `broadcast` | Optional: the broadcast that bare ids resolve against, so one layout fits any robot of a kind. |
| `grid` | `cols` (1–48) and `row_height` in px (8–400). Columns share the width. |
| `widgets` | The widgets, in reading order (narrow screens stack them in this order). |

A widget:

| Field | |
| --- | --- |
| `id` | Unique in the layout. |
| `type` | One of the types below. Unknown types load and show a placeholder, so a layout from a newer app still opens. |
| `at` | `[col, row, width, height]` in cells, inside `cols`. |
| `bind` | `track`, `channel`, `lock`: a path `<broadcast>/<id>` or a bare id. `path`: a field in a JSON data track's messages (`battery.voltage`, `cells.0`). `broadcast`: for widgets about a whole broadcast, and the broadcast bare ids in this widget resolve against. |
| `props` | Type-specific settings; defaults below. |
| `pinned` | Cannot be moved, resized, reconfigured or removed in the editor. |

A bare id resolves against the widget's `bind.broadcast`, then the layout's
`broadcast`, then the first broadcast in the room that has one by that id.

## Widget types

| Type | Binds | Props (default) | Cost |
| --- | --- | --- | --- |
| `video` | `track` (video) | `hud`: `full`, `compact`, `off` (`full`) | a decode; every frame, drawn in the media worker |
| `latency` | `track` (video) | `scale_ms` (100) | 2 Hz |
| `number` | `track`, `path` | `label`, `unit` (the track's `meta.units`), `digits` (2), `max_rate_hz` (30) | on change |
| `plot` | `track`, `path` | `label`, `window_s` (10), `min`, `max` (auto), `max_rate_hz` (0 = all) | once per display frame while data arrives |
| `values` | `track` | `max_rate_hz` (10) | on change; every field of the newest message |
| `button` | `channel` | `label`, `message` (`{}`) | on click |
| `toggle` | `channel`; optional `track` + `path` for the device's own state | `label`, `field` (the channel schema's one `bool`, else `on`) | on change |
| `slider` | `channel` | `label`, `field` (the schema's one `number`, else `value`), `min` (0), `max` (1), `step` (0.01), `rate_hz` (10) | on input |
| `drive` | `channel`, `lock` | `rate_hz` (20), `input`: `both`, `keyboard`, `gamepad` (web only for now), `command` (`drive`) | at its send rate |
| `lock` | `lock` | | on change |
| `status` | `broadcast` | | on change; its `.status` (dead-man, counters) |
| `log` | | | on new lines: room events, `.events`, remote stop results |

`map` is in the plan and not built yet.

## Rules the apps keep

- **Subscriptions follow widgets.** An app subscribes to the tracks its
  visible widgets are bound to, one subscription per track: if any widget
  wants every message the subscription has no `max_rate_hz`, else it takes
  the highest rate asked for. A widget scrolled out of view, or a hidden
  tab, unsubscribes. `.status` and `.events` are always subscribed.
- **Ingest rate is not render rate.** Messages are taken at their arrival
  rate and drawn at most once per display frame. The web worker batches
  them to the page at most every 8 ms; widgets mark themselves dirty and
  draw on the next animation frame. tq-app keeps each track's last 4096
  messages; a coalesced wake-up notifies only the widgets bound to tracks
  with new ones, and GPUI renders a notified view once per frame.
- **Video has a protected path.** On the web, frames are decoded and drawn
  by the media worker straight onto each video widget's canvas; nothing a
  widget does on the page can delay one. In tq-app, a decoded picture
  re-renders only its tile's picture view, and the path from the root to
  it has no cached views (which would re-render everything inside them).
  Both apps show their own draw time per frame (`ui … ms/frame`, `UI
  frame … ms`).
- **Costs are visible.** The editor shows each type's cost, counts video
  decodes and draws, and flags what is expensive or wrong: the same video
  in several widgets, more than four videos at full rate, a plot on a fast
  track without `max_rate_hz`, bindings not in the room, controls a viewer
  cannot use.
- **Permissions decide what binds.** The editor offers only the channels a
  session may `write` and the locks it may `lock`; controls without the
  permission (or the lock a channel needs) are disabled and say why.
- **The remote stop is not a widget.** It sits in the app's own header
  whenever the session may send, with Space as its key, and no layout can
  remove it. It stops every robot in the room and lets go of the drive
  locks this console holds (see the roadmap). In the web console Space is
  STOP even inside text fields.

## Storage

tq-app reads a layout from `--layout <file>` or the settings file's
`layout`, and refuses a file with problems, listing them all. It does not
edit layouts yet; one made in the web editor can be exported there and
given to it. Without one, it shows the preset.

The web console keeps the user's layout in `localStorage` under
`tq.layout.<room>.<preset>`. Without one it makes a preset from the room
(`presetLayout`): every video, then per broadcast the lock and drive
controls (operators, where there is a `drive` lock), its status and a
`values` table per data track, then a latency widget and the log. Preset
widget ids are stable, so the preset follows the room without restarting
video. Export and Import in the editor move layouts between browsers. Per-room
layouts kept on the relay come later.
