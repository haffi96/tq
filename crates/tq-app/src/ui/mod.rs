//! The window: a dock console. Title bar on top, the camera grid in the
//! middle with the events panel under it, a resizable sidebar with the
//! operator controls, latency and link on the right, a status bar below.
//!
//! Every part is a cached view. A decoded picture notifies only its tile,
//! so a video frame re-renders one tile and replays the rest; the panels
//! re-render on control messages, key presses and the stats tick. A cached
//! view is laid out from the style it is given, not from its content, so
//! each one gets a definite size.

mod model;
mod panels;
mod tile;
mod widgets;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{channel::mpsc::UnboundedReceiver, StreamExt};
use gpui_kit::{
    canvas,
    component::{h_resizable, resizable_panel, v_flex, v_resizable, ActiveTheme as _, TITLE_BAR_HEIGHT},
    prelude::*,
    px, AnyView, Context, Entity, FocusHandle, KeyDownEvent, KeyUpEvent, StyleRefinement, Window,
};

use self::{model::Model, panels::*, tile::Grid};
use crate::{
    session::{Event, Shared},
    update,
};

const STATS_INTERVAL: Duration = Duration::from_millis(500);

pub struct AppView {
    focus: FocusHandle,
    model: Entity<Model>,
    header: Entity<Header>,
    grid: Entity<Grid>,
    sidebar: Entity<Sidebar>,
    events: Entity<Events>,
    status: Entity<Status>,
    /// Render start -> the last element painted, per window frame.
    frame_start: Rc<Cell<Option<Instant>>>,
    frame_cost: Rc<RefCell<crate::stats::Win>>,
}

impl AppView {
    pub fn new(
        shared: Arc<Shared>,
        mut events: UnboundedReceiver<Event>,
        updates: update::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let model = cx.new(|_| Model::new(shared, updates));
        model.update(cx, |m, cx| m.check_update(cx));
        let view = Self {
            focus,
            header: cx.new(|cx| Header::new(model.clone(), cx)),
            grid: cx.new(|cx| Grid::new(model.clone(), cx)),
            sidebar: cx.new(|cx| Sidebar::new(model.clone(), cx)),
            events: cx.new(|cx| Events::new(model.clone(), cx)),
            status: cx.new(|cx| Status::new(model.clone(), cx)),
            model,
            frame_start: Default::default(),
            frame_cost: Default::default(),
        };

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let ok = this.update(cx, |view, cx| match event {
                    Event::Frame => {
                        view.model.read(cx).shared.frame_taken();
                        view.grid.update(cx, |g, cx| g.on_frame(cx));
                    }
                    event => view.model.update(cx, |m, cx| m.on_event(event, cx)),
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(STATS_INTERVAL).await;
            let ok = this.update(cx, |view, cx| {
                let cost = view.frame_cost.borrow_mut().take();
                view.model.update(cx, |m, cx| m.tick(cost, cx));
            });
            if ok.is_err() {
                break;
            }
        })
        .detach();

        // Keys held while the window loses focus never see their key-up.
        cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.model.update(cx, |m, cx| {
                    m.keys.clear();
                    m.update_drive(cx);
                });
            }
        })
        .detach();

        view
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).operator() || e.keystroke.modifiers.platform || e.keystroke.modifiers.control {
            return;
        }
        let key = match e.keystroke.key.as_str() {
            "w" => "w",
            "a" => "a",
            "s" => "s",
            "d" => "d",
            "space" => {
                self.model.update(cx, |m, cx| m.estop(cx));
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        self.model.update(cx, |m, cx| {
            if m.keys.insert(key) {
                m.update_drive(cx);
            }
        });
        cx.stop_propagation();
    }

    fn on_key_up(&mut self, e: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let before = m.keys.len();
            m.keys.retain(|k| *k != e.keystroke.key.as_str());
            if m.keys.len() != before {
                m.update_drive(cx);
            }
        });
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_start.set(Some(Instant::now()));
        let (sidebar, bottom) = {
            let m = self.model.read(cx);
            (m.sidebar, m.bottom)
        };
        let fill = || StyleRefinement::default().size_full();
        let (start, cost) = (self.frame_start.clone(), self.frame_cost.clone());
        v_flex()
            .id("root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(AnyView::from(self.header.clone()).cached(StyleRefinement::default().w_full().h(TITLE_BAR_HEIGHT)))
            .child(
                gpui_kit::div().flex_1().min_h_0().w_full().child(
                    h_resizable("dock")
                        .child(
                            resizable_panel().child(
                                v_resizable("center")
                                    .child(resizable_panel().child(self.grid.clone()))
                                    .child(
                                        resizable_panel()
                                            .size(px(160.))
                                            .size_range(px(80.)..px(480.))
                                            .visible(bottom)
                                            .child(AnyView::from(self.events.clone()).cached(fill())),
                                    ),
                            ),
                        )
                        .child(
                            resizable_panel()
                                .size(px(340.))
                                .size_range(px(260.)..px(560.))
                                .visible(sidebar)
                                .child(AnyView::from(self.sidebar.clone()).cached(fill())),
                        ),
                ),
            )
            .child(AnyView::from(self.status.clone()).cached(StyleRefinement::default().w_full().h(px(26.))))
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, _, _| {
                        if let Some(t) = start.take() {
                            let ms = t.elapsed().as_secs_f64() * 1000.0;
                            tracing::trace!(target: "tq_app::frame", ms, "frame");
                            cost.borrow_mut().add(ms);
                        }
                    },
                )
                .absolute(),
            )
    }
}
