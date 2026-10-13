//! The camera grid. Each camera is two views: [`Video`], the picture alone,
//! which a decoded frame re-renders, and [`Hud`], the chips and controls
//! over it, which re-renders with the 2 Hz stats. A video frame re-renders
//! the views on the path to its picture and replays everything else from
//! the last frame, so the UI adds as little as it can to decoded -> painted.

use std::sync::Arc;

use gpui_kit::{
    canvas,
    component::{
        button::{Button, ButtonVariants as _},
        h_flex, v_flex, ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    },
    div, hsla,
    prelude::*,
    px, surface, AnyView, App, Context, Entity, ObjectFit, SharedString, StyleRefinement, Window,
};

use super::{model::Model, widgets::*};
use crate::{
    feed::{Feed, Stats},
    session::Shared,
};

/// The newest picture of one camera.
pub struct Video {
    feed: Arc<Feed>,
}

impl Video {
    /// A new picture: re-render if this camera has one the UI has not
    /// painted yet.
    pub fn on_frame(&mut self, cx: &mut Context<Self>) {
        if self.feed.has_new_picture() {
            cx.notify();
        }
    }
}

impl Render for Video {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut video = div().size_full();
        if let Some(picture) = self.feed.picture() {
            let painted = self.feed.clone();
            video = video
                .child(surface(picture.0).object_fit(ObjectFit::Contain).size_full())
                .child(canvas(|_, _, _| {}, move |_, _, _, _| painted.mark_painted()).absolute());
        }
        video
    }
}

/// Everything drawn over a camera's picture.
pub struct Hud {
    shared: Arc<Shared>,
    feed: Arc<Feed>,
    stats: Option<Stats>,
    sync: Option<f64>,
    details: bool,
}

/// Translucent chip over the video; the same in light and dark themes.
fn chip() -> gpui_kit::Div {
    h_flex()
        .gap_1p5()
        .items_center()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(hsla(0., 0., 0., 0.55))
        .text_color(hsla(0., 0., 1., 0.92))
        .text_xs()
}

impl Render for Hud {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let feed = &self.feed;
        let live = self.stats.as_ref().is_some_and(|s| s.fps >= 0.5);
        let latency = self.stats.as_ref().and_then(|s| s.capture_to_render.avg);
        let level = if live {
            Level::of_ms(latency, 50., 100.)
        } else {
            Level::Unknown
        };
        let has_picture = feed.picture().is_some();
        let group = SharedString::from(format!("tile-{}", feed.info.id));
        let theme = cx.theme();

        let mut tile = div()
            .id(group.clone())
            .group(group.clone())
            .relative()
            .size_full()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if live { theme.border } else { theme.danger.opacity(0.6) });

        if !has_picture || !live {
            let text = if !has_picture {
                "Waiting for keyframe"
            } else {
                "No video"
            };
            tile = tile.child(
                v_flex()
                    .absolute()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_sm()
                    .text_color(hsla(0., 0., 1., 0.6))
                    .child(Icon::new(gpui_kit::assets::IconName::CameraOff).size_6())
                    .child(text),
            );
        }

        // Top: camera name and format, latency and frame rate.
        let format = self.stats.as_ref().map(|s| {
            let size = s.size.map_or(String::new(), |(w, h)| format!(" {w}×{h}"));
            format!("{}{size}", codec_name(s.codec.as_deref()))
        });
        tile = tile.child(
            h_flex()
                .absolute()
                .top_2()
                .left_2()
                .right_2()
                .justify_between()
                .child(
                    chip()
                        .child(div().font_semibold().child(feed.info.label.clone()))
                        .children(format.map(|f| div().opacity(0.7).child(f))),
                )
                .child(
                    chip()
                        .font_family(theme.mono_font_family.clone())
                        .child(div().size(px(7.)).rounded_full().bg(level.color(cx)))
                        .child(format!("{} ms", fmt(latency.filter(|_| live), 0)))
                        .child(
                            div()
                                .opacity(0.6)
                                .child(format!("{:.0} fps", self.stats.as_ref().map_or(0.0, |s| s.fps))),
                        ),
                ),
        );

        if self.details {
            if let Some(s) = &self.stats {
                tile = tile.child(
                    div()
                        .absolute()
                        .top(px(40.))
                        .left_2()
                        .p_2()
                        .rounded_md()
                        .bg(hsla(0., 0., 0., 0.65))
                        .text_color(hsla(0., 0., 1., 0.92))
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .line_height(px(16.))
                        .child(details_text(s, self.sync)),
                );
            }
        }

        // Bottom right, on hover: detail overlay and keyframe request.
        let id = feed.info.id.clone();
        let shared = self.shared.clone();
        tile.child(
            chip()
                .absolute()
                .bottom_2()
                .right_2()
                .gap_0p5()
                .px_0p5()
                .invisible()
                .group_hover(group, |s| s.visible())
                .child(
                    Button::new("details")
                        .icon(IconName::Info)
                        .xsmall()
                        .ghost()
                        .toggled(self.details)
                        .tooltip("Latency breakdown")
                        .on_click(cx.listener(|t, _, _, cx| {
                            t.details = !t.details;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("keyframe")
                        .icon(IconName::RefreshCw)
                        .xsmall()
                        .ghost()
                        .tooltip("Request keyframe")
                        .on_click(move |_, _, _: &mut App| shared.request_keyframe(&id, "user")),
                ),
        )
    }
}

pub struct Grid {
    model: Entity<Model>,
    tiles: Vec<(Entity<Video>, Entity<Hud>)>,
}

impl Grid {
    pub fn new(model: Entity<Model>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |grid, _, cx| grid.sync(cx)).detach();
        let mut grid = Self {
            model,
            tiles: Vec::new(),
        };
        grid.sync(cx);
        grid
    }

    pub fn on_frame(&mut self, cx: &mut Context<Self>) {
        if self.tiles.len() != self.model.read(cx).shared.feeds.lock().unwrap().len() {
            self.sync(cx);
        }
        for (video, _) in &self.tiles {
            video.update(cx, |v, cx| v.on_frame(cx));
        }
    }

    /// Model changed: new cameras get tiles, every tile gets fresh stats.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let shared = model.shared.clone();
        let feeds = shared.feeds();
        let sync = model.shared.clock.lock().unwrap().sync_error_ms();
        let stats: Vec<_> = feeds
            .iter()
            .map(|f| {
                model
                    .stats
                    .iter()
                    .find(|(id, _)| *id == f.info.id)
                    .map(|(_, s)| s.clone())
            })
            .collect();
        if feeds.len() != self.tiles.len() {
            self.tiles = feeds
                .iter()
                .map(|feed| {
                    let video = cx.new(|_| Video { feed: feed.clone() });
                    let hud = cx.new(|_| Hud {
                        shared: shared.clone(),
                        feed: feed.clone(),
                        stats: None,
                        sync: None,
                        details: false,
                    });
                    (video, hud)
                })
                .collect();
            cx.notify();
        }
        for ((_, hud), stats) in self.tiles.iter().zip(stats) {
            hud.update(cx, |t, cx| {
                t.stats = stats;
                t.sync = sync;
                cx.notify();
            });
        }
    }
}

impl Render for Grid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cols = (self.tiles.len() as f64).sqrt().ceil().max(1.0) as usize;
        let mut grid = v_flex().size_full().gap_2().p_2().bg(cx.theme().background);
        if self.tiles.is_empty() {
            grid = grid
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(gpui_kit::assets::IconName::Cctv).size_8())
                .child("Waiting for the publisher…");
        }
        for row in self.tiles.chunks(cols) {
            let mut r = h_flex().flex_1().gap_2().min_h_0();
            for (video, hud) in row {
                let layer = || StyleRefinement::default().absolute().size_full();
                r = r.child(
                    div()
                        .relative()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .overflow_hidden()
                        .rounded(cx.theme().radius_lg)
                        .bg(gpui_kit::black())
                        .child(AnyView::from(video.clone()).cached(layer()))
                        .child(AnyView::from(hud.clone()).cached(layer())),
                );
            }
            grid = grid.child(r);
        }
        grid
    }
}

fn codec_name(codec: Option<&str>) -> &'static str {
    match codec {
        Some(c) if c.starts_with("avc") => "H.264",
        Some(c) if c.starts_with("hev") || c.starts_with("hvc") => "H.265",
        Some(_) => "video",
        None => "",
    }
}

fn details_text(s: &Stats, sync: Option<f64>) -> String {
    let sei = if s.capture_from_sei {
        ""
    } else {
        "  (no SEI: capture = publisher time)"
    };
    let mut lines = vec![
        format!(
            "{:.0} kbps  frames {}  drop {}  gops {}  err {}",
            s.kbps, s.frames, s.dropped, s.groups, s.errors
        ),
        format!("ms avg/max{sei}"),
        format!("capture→pub      {}", s.capture_to_publisher),
        format!("pub→relay        {}", s.publisher_to_relay),
        format!("relay→app        {}", s.relay_to_app),
        format!("decode           {}", s.decode),
        format!("capture→decoded  {}", s.capture_to_decoded),
        format!("decoded→painted  {}", s.decoded_to_painted),
        format!("capture→render   {}  ±{} sync", s.capture_to_render, fmt(sync, 1)),
    ];
    if let Some(e) = &s.last_error {
        lines.push(format!("last error: {e}"));
    }
    lines.join("\n")
}

/// One line per camera for `--log-stats`.
pub fn overlay_text(s: &Stats, sync: Option<f64>) -> String {
    let size = s.size.map_or("–".to_string(), |(w, h)| format!("{w}x{h}"));
    format!(
        "{} {size}\n{:.1} fps  {:.0} kbps\n{}",
        s.codec.as_deref().unwrap_or("–"),
        s.fps,
        s.kbps,
        details_text(s, sync)
    )
}
