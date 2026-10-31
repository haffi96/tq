//! The panels around the board: title bar (with the remote stop), the
//! sidebar with teleop numbers, latency and link, the events panel and the
//! status bar. Each
//! re-renders only when the [`Model`] changes (room states and events, key
//! presses, the 2 Hz stats tick), never for a video frame.

use gpui_kit::{
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        tag::Tag,
        v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme, ThemeMode,
        TitleBar,
    },
    div,
    prelude::*,
    px, AnyElement, App, Context, Entity, SharedString, Window,
};
use tq_wire::{BuildInfo, Compat};

use super::{
    model::{kind_of, time_of_day, Model, UpdateState},
    widgets::*,
};
use crate::session::ConnState;

macro_rules! panel {
    ($name:ident) => {
        pub struct $name {
            model: Entity<Model>,
        }

        impl $name {
            pub fn new(model: Entity<Model>, cx: &mut Context<Self>) -> Self {
                cx.observe(&model, |_, _, cx| cx.notify()).detach();
                Self { model }
            }
        }
    };
}

panel!(Header);
panel!(Sidebar);
panel!(Events);
panel!(Status);

fn conn(m: &Model, cx: &App) -> (String, gpui_kit::Hsla) {
    let t = cx.theme();
    match &m.conn {
        (ConnState::Connected, _) => ("Connected".into(), t.success),
        (ConnState::Connecting, _) => ("Connecting…".into(), t.warning),
        (ConnState::Closed, d) => (format!("Disconnected {}", d.as_deref().unwrap_or("")), t.danger),
    }
}

// --- title bar ----------------------------------------------------------

impl Render for Header {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.clone();
        let m = self.model.read(cx);
        let c = &m.shared.config;
        let (conn_text, conn_color) = conn(m, cx);
        let others = m.state.as_ref().map_or(0, |s| s.broadcasts.len().saturating_sub(1));
        let more = if others > 0 {
            format!("  +{others}")
        } else {
            String::new()
        };
        let pub_status = match m.main_broadcast() {
            None => status("No broadcasts".to_string(), cx.theme().danger),
            Some(b) if b.owner.is_none() => status(format!("{} offline{more}", b.name), cx.theme().danger),
            Some(b) => status(
                format!("{} · {} ms{more}", b.name, fmt(b.rtt_ms, 1)),
                cx.theme().success,
            ),
        };
        let dark = cx.theme().mode.is_dark();
        let (sidebar, bottom) = (m.sidebar, m.bottom);
        // An available update shows here too, not only at the bottom of the
        // sidebar; refused while driving (the restart drops the session).
        let update = match &m.update {
            UpdateState::Available(r) => {
                let model = model.clone();
                let driving = m.driving();
                Some(
                    Button::new("update")
                        .small()
                        .primary()
                        .icon(IconName::ArrowUp)
                        .label(format!("Update to {}", r.version))
                        .disabled(driving)
                        .tooltip(if driving {
                            "Release the claim first"
                        } else {
                            "Download, verify and install, then restart"
                        })
                        .on_click(move |_, _, cx| model.update(cx, |m, cx| m.install_update(cx))),
                )
            }
            _ => None,
        };
        let toggle = |id: &'static str, icon: IconName, on: bool, tip: &'static str, f: fn(&mut Model)| {
            let model = model.clone();
            Button::new(id)
                .icon(icon)
                .small()
                .ghost()
                .toggled(on)
                .tooltip(tip)
                .on_click(move |_, _, cx| {
                    model.update(cx, |m, cx| {
                        f(m);
                        cx.notify();
                    })
                })
        };

        TitleBar::new().child(
            h_flex()
                .flex_1()
                .justify_between()
                .items_center()
                .pr_2()
                .text_sm()
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(Icon::new(gpui_kit::assets::IconName::CarFront).small())
                                .child(div().font_semibold().child(c.room.clone())),
                        )
                        .child(
                            if m.operator() {
                                Tag::primary().child("Operator")
                            } else {
                                Tag::secondary().child("Viewer")
                            }
                            .xsmall(),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(status(conn_text, conn_color)),
                        )
                        .child(div().text_color(cx.theme().muted_foreground).child(pub_status)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .when(m.operator(), |row| {
                            let model = model.clone();
                            row.child(
                                Button::new("header-stop")
                                    .danger()
                                    .small()
                                    .icon(gpui_kit::assets::IconName::OctagonX)
                                    .label("STOP")
                                    .tooltip("Every robot in the room safe-stops, whoever drives it, until its drive lock is claimed again (Space)")
                                    .on_click(move |_, _, cx| model.update(cx, |m, cx| m.remote_stop(cx))),
                            )
                        })
                        .children(update)
                        .child(toggle(
                            "toggle-bottom",
                            IconName::PanelBottom,
                            bottom,
                            "Events panel",
                            |m| m.bottom = !m.bottom,
                        ))
                        .child(toggle(
                            "toggle-sidebar",
                            IconName::PanelRight,
                            sidebar,
                            "Sidebar",
                            |m| m.sidebar = !m.sidebar,
                        ))
                        .child(
                            Button::new("theme")
                                .icon(if dark { IconName::Sun } else { IconName::Moon })
                                .small()
                                .ghost()
                                .tooltip(if dark { "Light theme" } else { "Dark theme" })
                                .on_click(move |_, window, cx| {
                                    let mode = if dark { ThemeMode::Light } else { ThemeMode::Dark };
                                    Theme::change(mode, Some(window), cx);
                                }),
                        ),
                ),
        )
    }
}

// --- sidebar ------------------------------------------------------------

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        let mut col = v_flex().gap_5().p_4();
        if m.operator() {
            col = col.child(self.control(cx));
        }
        col = col.child(self.latency(cx)).child(self.link(cx)).child(self.about(cx));
        div()
            .id("sidebar")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().sidebar)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(col)
    }
}

impl Sidebar {
    /// The remote stop and teleop's numbers. Claiming and driving are the
    /// layout's lock and drive widgets; the stop is not part of any layout.
    fn control(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.clone();
        let m = self.model.read(cx);
        let t = m.shared.teleop.lock().unwrap();
        let mut acks: Vec<_> = t.acks.iter().map(|(k, v)| format!("{k} {v}")).collect();
        acks.sort();
        let acks = if acks.is_empty() {
            "–".to_string()
        } else {
            acks.join(" · ")
        };
        let robot = match t.tripped {
            None => "–",
            Some(true) => "tripped",
            Some(false) => "armed",
        };
        let last_ack = t.last_ack_detail.clone();
        drop(t);
        v_flex()
            .gap_3()
            .child(section("Teleop", None, cx))
            .child(
                Button::new("remote-stop")
                    .danger()
                    .w_full()
                    .icon(gpui_kit::assets::IconName::OctagonX)
                    .label("STOP  ·  Space")
                    .on_click(move |_, _, cx| model.update(cx, |m, cx| m.remote_stop(cx))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(kv("ack rtt", format!("{} ms", m.teleop_rtt.0), cx))
                    .child(kv("heartbeat rtt", format!("{} ms", m.teleop_rtt.1), cx))
                    .child(kv("acks", acks, cx))
                    .child(kv("robot (heartbeat echo)", robot, cx))
                    .children(last_ack.map(|d| kv("last ack", d, cx))),
            )
            .into_any_element()
    }

    fn latency(&self, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let mut col = v_flex().gap_4().child(section(
            "Glass latency",
            Some(div().child("last 60 s").into_any_element()),
            cx,
        ));
        for feed in m.shared.feeds() {
            let id = &feed.path.to_string();
            let stats = m.stats.iter().find(|(i, _)| i == id).map(|(_, s)| s);
            let live = stats.is_some_and(|s| s.fps >= 0.5);
            let render = stats.and_then(|s| s.capture_to_render.avg).filter(|_| live);
            let ui = stats.and_then(|s| s.decoded_to_painted.avg).filter(|_| live);
            let color = Level::of_ms(render, 50., 100.).color(cx);
            let history: Vec<_> = m.history.get(id).map_or(Vec::new(), |h| h.iter().copied().collect());
            col = col.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_end()
                            .child(metric(SharedString::from(feed.label.clone()), render, "ms", color, cx))
                            .child(
                                v_flex()
                                    .items_end()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("in app {} ms", fmt(ui, 1)))
                                    .child(format!("{:.0} fps", stats.map_or(0.0, |s| s.fps))),
                            ),
                    )
                    .child(sparkline(format!("lat-{id}"), history, color)),
            );
        }
        col.into_any_element()
    }

    fn link(&self, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let broadcast = m.main_broadcast();
        let pub_rtt = broadcast.and_then(|b| b.rtt_ms);
        let relay_rtt = m.shared.relay_rtt_ms();
        let sync = broadcast.and_then(|b| m.shared.sync_error_ms(&b.name));
        let history: Vec<_> = m.rtt_history.iter().copied().collect();
        let mut col = v_flex()
            .gap_1()
            .child(section(
                "Link",
                broadcast.map(|b| div().child(b.name.clone()).into_any_element()),
                cx,
            ))
            .child(
                h_flex()
                    .gap_6()
                    .pt_1()
                    .child(metric(
                        "Robot RTT",
                        pub_rtt,
                        "ms",
                        Level::of_ms(pub_rtt, 80., 200.).color(cx),
                        cx,
                    ))
                    .child(metric("Relay RTT", relay_rtt, "ms", cx.theme().foreground, cx)),
            )
            .child(sparkline("link-rtt", history, cx.theme().chart_2))
            .child(kv("clock sync", format!("±{} ms", fmt(sync, 1)), cx));
        if let Some(q) = broadcast.and_then(|b| b.quic.as_ref()) {
            col = col
                .child(kv(
                    "uplink rtt / cwnd",
                    format!("{:.1} ms / {} KiB", q.rtt_ms, q.cwnd / 1024),
                    cx,
                ))
                .child(kv(
                    "uplink lost",
                    format!("{} / {}  cong {}", q.lost_packets, q.sent_packets, q.congestion_events),
                    cx,
                ));
        }
        if let Some(l) = &m.link {
            col = col.child(kv("downlink", l.clone(), cx));
        }
        col.into_any_element()
    }
}

impl Sidebar {
    /// Versions of the app, the relay and each broadcast's owner, and
    /// updates.
    fn about(&self, cx: &mut Context<Self>) -> AnyElement {
        let model = self.model.clone();
        let m = self.model.read(cx);
        let connected = m.session.is_some();
        let app = &m.shared.config.build;
        let mut col = v_flex()
            .gap_1()
            .child(section("About", None, cx))
            .child(version_row("app", app.long(), Compat::Same, cx))
            .child(match (connected, &m.relay_build) {
                (false, _) => version_row("relay", "–".into(), Compat::Same, cx),
                (true, b) => version_row("relay", build_text(b.as_ref()), Compat::of_build(b.as_ref()), cx),
            });
        for b in m.state.iter().flat_map(|s| &s.broadcasts) {
            col = col.child(match (&b.owner, b.build.as_ref()) {
                (None, _) => version_row(b.name.clone(), "offline".into(), Compat::Same, cx),
                (Some(_), build) => version_row(b.name.clone(), build_text(build), Compat::of_build(build), cx),
            });
        }

        let (status_text, status_color) = match &m.update {
            UpdateState::Idle => (String::new(), cx.theme().muted_foreground),
            UpdateState::Checking => ("Checking…".into(), cx.theme().muted_foreground),
            UpdateState::UpToDate => (
                format!("{} is the newest release", app.version),
                cx.theme().muted_foreground,
            ),
            UpdateState::Available(r) => (format!("{} is available", r.version), cx.theme().primary),
            UpdateState::Installing => (
                "Installing; the app restarts when done".into(),
                cx.theme().muted_foreground,
            ),
            UpdateState::Failed(e) => (e.clone(), cx.theme().danger),
        };
        let busy = matches!(m.update, UpdateState::Checking | UpdateState::Installing);
        let mut buttons = h_flex().gap_2().pt_2().child(
            Button::new("check-update")
                .small()
                .outline()
                .label("Check for updates")
                .loading(matches!(m.update, UpdateState::Checking))
                .disabled(busy)
                .on_click({
                    let model = model.clone();
                    move |_, _, cx| model.update(cx, |m, cx| m.check_update(cx))
                }),
        );
        if let UpdateState::Available(r) = &m.update {
            let driving = m.driving();
            buttons = buttons.child(
                Button::new("install-update")
                    .small()
                    .primary()
                    .label(format!("Install {} and restart", r.version))
                    .disabled(driving)
                    .tooltip(if driving {
                        "Release the claim first"
                    } else {
                        "Downloads, verifies and replaces the app, then starts it again"
                    })
                    .on_click(move |_, _, cx| model.update(cx, |m, cx| m.install_update(cx))),
            );
        }
        col = col.child(buttons);
        if !status_text.is_empty() {
            col = col.child(div().text_xs().text_color(status_color).child(status_text));
        }
        col.into_any_element()
    }
}

fn build_text(b: Option<&BuildInfo>) -> String {
    b.map_or_else(
        || "unknown".into(),
        |b| format!("{} · protocol {}", b.short(), b.protocol),
    )
}

/// `label ..... version`, in the warning color when the protocol minor
/// differs from the app's.
fn version_row(label: impl Into<SharedString>, text: String, compat: Compat, cx: &App) -> impl IntoElement {
    let t = cx.theme();
    let color = match compat {
        Compat::Same => t.foreground,
        Compat::MinorDiffers => t.warning,
        Compat::Incompatible => t.danger,
    };
    h_flex()
        .justify_between()
        .gap_2()
        .text_xs()
        .child(div().text_color(t.muted_foreground).child(label.into()))
        .child(
            div()
                .font_family(t.mono_font_family.clone())
                .text_color(color)
                .child(text),
        )
}

// --- events panel -------------------------------------------------------

impl Render for Events {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        let t = cx.theme();
        let mut cams = v_flex().gap_1().child(section("Tracks", None, cx));
        let tracks: Vec<_> = m
            .state
            .iter()
            .flat_map(|s| &s.broadcasts)
            .flat_map(|b| b.tracks.iter().map(move |tr| (b, tr)))
            .collect();
        for (b, tr) in &tracks {
            let st = &tr.stats;
            let mut row = h_flex()
                .gap_3()
                .text_xs()
                .font_family(t.mono_font_family.clone())
                .child(status(
                    format!("{}/{}", b.name, tr.info.id),
                    if tr.online { t.success } else { t.danger },
                ))
                .child(
                    div().text_color(t.muted_foreground).child(
                        st.codec_string
                            .clone()
                            .unwrap_or_else(|| format!("{:?}", tr.info.kind).to_lowercase()),
                    ),
                )
                .child(format!("{:.1} fps", st.fps))
                .child(format!("{:.0} kbps", st.kbps))
                .child(format!("drops {}", st.dropped));
            if st.capture_to_publisher_ms.is_some() {
                row = row.child(format!("capture→pub {} ms", fmt(st.capture_to_publisher_ms, 1)));
            }
            cams = cams.child(row);
        }
        if tracks.is_empty() {
            cams = cams.child(div().text_xs().text_color(t.muted_foreground).child("–"));
        }
        let mut log = v_flex().gap_0p5().child(section("Events", None, cx));
        if m.log.is_empty() {
            log = log.child(div().text_xs().text_color(t.muted_foreground).child("Nothing yet"));
        }
        for (at_us, line) in &m.log {
            log = log.child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .child(
                        div()
                            .font_family(t.mono_font_family.clone())
                            .text_color(t.muted_foreground)
                            .child(time_of_day(*at_us)),
                    )
                    .child(line.clone()),
            );
        }
        h_flex()
            .size_full()
            .bg(t.background)
            .border_t_1()
            .border_color(t.border)
            .child(div().id("log").flex_1().h_full().overflow_y_scroll().p_3().child(log))
            .child(div().w(px(1.)).h_full().bg(t.border))
            .child(div().id("cams").flex_1().h_full().overflow_y_scroll().p_3().child(cams))
    }
}

// --- status bar ---------------------------------------------------------

impl Render for Status {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        let t = cx.theme();
        let (conn_text, conn_color) = conn(m, cx);
        let mut left = h_flex().gap_4().child(status(conn_text, conn_color));
        if let Some(s) = &m.session {
            left = left.child(format!("session {}", short(s)));
        }
        left = left
            .child(format!("relay {} ms", fmt(m.shared.relay_rtt_ms(), 1)))
            .child(format!("sync ±{} ms", fmt(m.shared.worst_sync_error_ms(), 1)));
        let mut right = h_flex().gap_4();
        if let UpdateState::Available(r) = &m.update {
            right = right.child(
                div()
                    .text_color(t.primary)
                    .child(format!("update {} available", r.version)),
            );
        }
        // Versions: the app, and what it is connected to. Amber when a
        // protocol minor differs (works, some features may be missing).
        let mut versions = h_flex().gap_1().child(format!("app {}", m.shared.config.build.version));
        if m.session.is_some() {
            let relay = m.relay_build.as_ref();
            versions = versions.child("·").child(
                div()
                    .when(Compat::of_build(relay) != Compat::Same, |d| d.text_color(t.warning))
                    .child(format!("relay {}", relay.map_or("?", |b| b.version.as_str()))),
            );
        }
        for b in m.state.iter().flat_map(|s| &s.broadcasts).filter(|b| b.owner.is_some()) {
            let p = b.build.as_ref();
            versions = versions.child("·").child(
                div()
                    .when(Compat::of_build(p) != Compat::Same, |d| d.text_color(t.warning))
                    .child(format!("{} {}", b.name, p.map_or("?", |b| b.version.as_str()))),
            );
        }
        right = right.child(versions);
        if let Some(s) = &m.state {
            let count = |k| s.participants.iter().filter(|p| kind_of(&p.perms) == k).count();
            right = right.child(format!(
                "{} operator · {} viewer · {} robot",
                count("operator"),
                count("viewer"),
                count("robot")
            ));
        }
        right = right.child(format!("UI frame {} ms", fmt(m.frame_cost.avg, 2)));
        h_flex()
            .size_full()
            .px_3()
            .justify_between()
            .items_center()
            .text_xs()
            .text_color(t.muted_foreground)
            .bg(t.status_bar)
            .border_t_1()
            .border_color(t.status_bar_border)
            .child(left)
            .child(right)
    }
}
