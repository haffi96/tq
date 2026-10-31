//! A video tile, as the board's video widgets show it: two views,
//! [`Video`], the picture alone, which a decoded frame re-renders, and
//! [`Hud`], the chips and controls over it, which re-renders with the 2 Hz
//! stats. A video frame re-renders
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
    px, App, Context, ObjectFit, SharedString, Window,
};

use super::widgets::*;
use crate::{
    decode::Picture,
    feed::{Feed, Stats},
    session::Shared,
};

/// The newest picture of one video track.
pub struct Video {
    feed: Arc<Feed>,
    /// The image in the window's sprite atlas. Each frame is a new image;
    /// the one it replaces has to leave the atlas or the atlas grows forever.
    #[cfg(target_os = "linux")]
    shown: Option<Arc<gpui_kit::RenderImage>>,
}

impl Video {
    pub fn new(feed: Arc<Feed>, cx: &mut Context<Self>) -> Self {
        #[cfg(target_os = "linux")]
        cx.on_release(|this, cx| {
            if let Some(image) = this.shown.take() {
                cx.drop_image(image, None);
            }
        })
        .detach();
        #[cfg(not(target_os = "linux"))]
        let _ = cx;
        Self {
            feed,
            #[cfg(target_os = "linux")]
            shown: None,
        }
    }

    /// A new picture: re-render if this track has one the UI has not
    /// painted yet.
    pub fn on_frame(&mut self, cx: &mut Context<Self>) {
        if self.feed.has_new_picture() {
            cx.notify();
        }
    }
}

impl Render for Video {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut video = div().size_full();
        if let Some(picture) = self.feed.picture() {
            let painted = self.feed.clone();
            video = video
                .child(self.picture(picture, window))
                .child(canvas(|_, _, _| {}, move |_, _, _, _| painted.mark_painted()).absolute());
        }
        video
    }
}

impl Video {
    #[cfg(target_os = "macos")]
    fn picture(&mut self, picture: Picture, _: &mut Window) -> impl IntoElement {
        gpui_kit::surface(picture.0).object_fit(ObjectFit::Contain).size_full()
    }

    #[cfg(target_os = "linux")]
    fn picture(&mut self, picture: Picture, window: &mut Window) -> impl IntoElement {
        use gpui_kit::StyledImage as _;
        let image = picture.0;
        if self.shown.as_ref().is_none_or(|shown| shown.id != image.id) {
            if let Some(old) = self.shown.replace(image.clone()) {
                let _ = window.drop_image(old);
            }
        }
        gpui_kit::img(image).object_fit(ObjectFit::Contain).size_full()
    }
}

/// Everything drawn over a track's picture.
pub struct Hud {
    shared: Arc<Shared>,
    feed: Arc<Feed>,
    stats: Option<Stats>,
    sync: Option<f64>,
    details: bool,
}

impl Hud {
    pub fn new(shared: Arc<Shared>, feed: Arc<Feed>) -> Self {
        Self {
            shared,
            feed,
            stats: None,
            sync: None,
            details: false,
        }
    }

    pub fn set_stats(&mut self, stats: Option<Stats>, sync: Option<f64>, cx: &mut Context<Self>) {
        self.stats = stats;
        self.sync = sync;
        cx.notify();
    }
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
        let group = SharedString::from(format!("tile-{}", feed.path));
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

        // Top: track name and format, latency and frame rate.
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
                        .child(div().font_semibold().child(feed.label.clone()))
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
        let path = feed.path.clone();
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
                        .on_click(move |_, _, _: &mut App| shared.request_keyframe(&path, "user")),
                ),
        )
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
        "  (no SEI: no capture times)"
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

/// One line per video track for `--log-stats`.
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
