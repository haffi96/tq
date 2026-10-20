//! Small shared pieces: status dots, labeled values, section headers,
//! sparklines, latency colors.

use gpui_kit::{
    component::{chart::LineChart, h_flex, v_flex, ActiveTheme as _, StyledExt as _},
    div,
    prelude::*,
    px, App, Hsla, SharedString,
};

/// How good a latency is, for its color. The budget is glass-to-glass for
/// driving: under 50 ms feels live, over 100 ms does not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Good,
    Fair,
    Poor,
    Unknown,
}

impl Level {
    pub fn of_ms(ms: Option<f64>, fair: f64, poor: f64) -> Self {
        match ms {
            None => Level::Unknown,
            Some(v) if v >= poor => Level::Poor,
            Some(v) if v >= fair => Level::Fair,
            Some(_) => Level::Good,
        }
    }

    pub fn color(self, cx: &App) -> Hsla {
        let t = cx.theme();
        match self {
            Level::Good => t.success,
            Level::Fair => t.warning,
            Level::Poor => t.danger,
            Level::Unknown => t.muted_foreground,
        }
    }
}

/// A colored dot and a label: connection and robot states.
pub fn status(label: impl Into<SharedString>, color: Hsla) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .items_center()
        .child(div().size(px(7.)).rounded_full().bg(color))
        .child(label.into())
}

/// Small caps heading for a sidebar section, with an optional right side.
pub fn section(title: &'static str, right: Option<gpui_kit::AnyElement>, cx: &App) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(title.to_uppercase())
        .children(right)
}

/// `label ........ value` row in monospace digits.
pub fn kv(label: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    h_flex()
        .justify_between()
        .text_xs()
        .child(div().text_color(cx.theme().muted_foreground).child(label.into()))
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .child(value.into()),
        )
}

/// A big number with its unit and caption: the headline latency figures.
pub fn metric(
    caption: impl Into<SharedString>,
    value: Option<f64>,
    unit: &'static str,
    color: Hsla,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(caption.into()),
        )
        .child(
            h_flex()
                .items_baseline()
                .gap_1()
                .child(
                    div()
                        .text_xl()
                        .font_semibold()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(color)
                        .child(fmt(value, 1)),
                )
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child(unit)),
        )
}

/// A line of the last minute of a value, no axes. Gaps (no sample) are
/// drawn as the previous value.
pub fn sparkline(id: impl Into<SharedString>, values: Vec<Option<f64>>, color: Hsla) -> impl IntoElement {
    let mut last = 0.0;
    let points: Vec<(SharedString, f64)> = values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            last = v.unwrap_or(last);
            (SharedString::from(i.to_string()), last)
        })
        .collect();
    let max = points.iter().map(|p| p.1).fold(1.0f64, f64::max);
    div().h(px(36.)).w_full().child(
        LineChart::new(points)
            .id(gpui_kit::ElementId::Name(id.into()))
            .x(|p| p.0.clone())
            .y(|p| p.1)
            .y_domain(0.0, max * 1.15)
            .stroke(color)
            .linear()
            .x_axis(false)
            .grid(false)
            .interactive(false)
            .appear(false),
    )
}

pub fn fmt(v: Option<f64>, digits: usize) -> String {
    v.map_or("–".to_string(), |v| format!("{v:.digits$}"))
}

pub fn short(s: &str) -> &str {
    &s[..s.len().min(8)]
}
