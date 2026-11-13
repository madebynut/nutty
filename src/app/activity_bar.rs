//! VS Code style activity bar: a fixed narrow column that opens the connection
//! sidebar and settings.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, button::ButtonVariants as _};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, div, prelude::*, px, rgb,
};

use super::{PanelEdge, WorkspaceView, panels};

/// Width of the fixed column at the window's left edge.
pub(super) const ACTIVITY_BAR: f32 = 48.;
/// Horizontal space macOS reserves for the traffic lights from the window's left edge.
const TRAFFIC_LIGHTS: f32 = 82.;
/// Extra left padding the first element right of the activity bar needs on macOS so it
/// clears the traffic lights, which extend past the bar.
pub(super) const TRAFFIC_LIGHT_OVERHANG: f32 = TRAFFIC_LIGHTS - ACTIVITY_BAR;
const ITEM: f32 = 40.;

type ClickHandler = Box<dyn Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App)>;
const REVEAL: Duration = Duration::from_millis(160);

impl WorkspaceView {
    /// Shows or hides the connection sidebar; opening it plays the slide-in.
    pub(super) fn toggle_sidebar(&mut self) {
        self.sidebar.toggle();
        if self.sidebar.open {
            self.sidebar_reveal = self.sidebar_reveal.wrapping_add(1);
        }
    }

    /// Wraps the sidebar so it slides in from the activity bar after being opened.
    /// The panel at launch (and while resizing) appears at its size immediately.
    pub(super) fn reveal_sidebar(&self, sidebar: gpui_kit::Div, width: f32) -> AnyElement {
        if self.sidebar_reveal == 0 {
            return sidebar.into_any_element();
        }
        sidebar
            .with_animation(
                format!("sidebar-reveal-{}", self.sidebar_reveal),
                Animation::new(REVEAL),
                move |sidebar, progress| {
                    let eased = 1. - (1. - progress).powi(3);
                    sidebar.w(px(width * eased))
                },
            )
            .into_any_element()
    }

    pub(super) fn render_activity_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        let connections_open = self.sidebar.extent(self.sidebar_limit(PanelEdge::Left)) > 0.;
        let item = |id: &'static str,
                    icon: IconName,
                    label: &str,
                    active: bool,
                    on_click: ClickHandler| {
            div()
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .w(px(ACTIVITY_BAR))
                .h(px(ITEM + 8.))
                .when(active, |item| {
                    item.child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(12.))
                            .w(px(2.))
                            .h(px(ITEM - 16.))
                            .rounded_sm()
                            .bg(rgb(p.accent)),
                    )
                })
                .child(
                    crate::controls::button(id)
                        .ghost()
                        .compact()
                        .w(px(ITEM))
                        .h(px(ITEM))
                        .accessibility_label(label.to_string())
                        .tooltip(label.to_string())
                        .child(Icon::new(icon).size_5().text_color(rgb(if active {
                            p.text
                        } else {
                            p.muted
                        })))
                        .on_click(move |event, window, cx| on_click(event, window, cx)),
                )
        };
        div()
            .id("activity-bar")
            .flex()
            .flex_col()
            .items_center()
            .justify_between()
            .w(px(ACTIVITY_BAR))
            .flex_shrink_0()
            .min_h_0()
            .bg(rgb(p.base))
            .border_r_1()
            .border_color(rgb(p.border))
            // The macOS traffic lights sit above the first item.
            .pt(px(if cfg!(target_os = "macos") {
                panels::HEADER_BAR
            } else {
                4.
            }))
            .pb_1()
            .child(item(
                "activity-connections",
                IconName::Server,
                crate::i18n::text("连接"),
                connections_open,
                Box::new(cx.listener(|view, _, window, cx| {
                    view.toggle_sidebar();
                    if !view.sidebar.open {
                        view.focus_terminal(window, cx);
                    }
                    cx.notify();
                })),
            ))
            .child(item(
                "activity-settings",
                IconName::Settings2,
                crate::i18n::text("设置"),
                false,
                Box::new(cx.listener(|view, _, window, cx| view.settings_form(window, cx))),
            ))
            .into_any_element()
    }
}
