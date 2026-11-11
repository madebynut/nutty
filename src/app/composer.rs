//! Pane-bound, transparent inputs that grow in place and preserve editing state.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{Icon, button::ButtonVariants as _, input::Textarea};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, Window, div, prelude::*, px,
    rgb,
};
use uuid::Uuid;

use super::{
    WorkspaceView, icon_button,
    panels::{DOCK_BAR, FOOTER_BAR},
};

const INPUT_PADDING: f32 = 16.;
const INPUT_ACTION: f32 = 28.;
const ACTION_INSET: f32 = (FOOTER_BAR - 1. - INPUT_ACTION) * 0.5;
const CONVERSATION_MIN: f32 = 48.;
const AGENT_TIPS: f32 = 24.;

#[derive(Debug)]
struct PromptLayout {
    height: f32,
    editor_height: f32,
    max_rows: usize,
}

impl PromptLayout {
    fn new(dock: f32, line_height: f32, rows: usize, expanded: bool) -> Self {
        let body = (dock - DOCK_BAR - AGENT_TIPS - 1.).max(1.);
        // Small horizontal splits still need a usable one-line input.
        let minimum = (line_height + INPUT_PADDING + 1.).max(FOOTER_BAR).min(body);
        let maximum = if expanded {
            body
        } else {
            (body - CONVERSATION_MIN).max(minimum)
        };
        let max_rows =
            ((maximum - INPUT_PADDING - 1.).max(line_height) / line_height).floor() as usize;
        let text_height = rows.max(1).min(max_rows) as f32 * line_height + INPUT_PADDING;
        let height = if expanded {
            maximum
        } else {
            (text_height + 1.).max(FOOTER_BAR).min(maximum)
        };
        Self {
            height,
            // The transparent editor stays centred in the folded-height frame.
            // Expanded inputs fill their viewport and keep long text scrollable.
            editor_height: if expanded {
                height - 1.
            } else {
                text_height.min(height - 1.)
            },
            max_rows,
        }
    }
}

impl WorkspaceView {
    fn toggle_prompt_expansion(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.assistant.session.ended
            || pane.assistant.session.pending.is_some()
            || pane.assistant.session.tool_running()
            || pane.view.read(cx).snapshot.closed
        {
            return;
        }
        pane.assistant.prompt_expanded = !pane.assistant.prompt_expanded;
        pane.assistant
            .prompt
            .update(cx, |input, cx| input.focus(window, cx));
        self.activate_pane(id, cx);
        cx.notify();
    }

    pub(super) fn render_command_sender(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let palette = self.palette();
        let closed = pane.view.read(cx).snapshot.closed;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div().flex_1().min_h_0().min_w_0().child(
                    Textarea::new(&pane.assistant.command)
                        .appearance(false)
                        .bordered(false)
                        .rounded_none()
                        .font(
                            crate::fonts::FontPair::terminal(&self.store.profile.preferences, cx)
                                .latin,
                        )
                        .text_size(px(self.store.profile.preferences.font_size))
                        .aria_label(crate::i18n::text("命令发送器"))
                        .w_full()
                        .h(gpui_kit::relative(1.))
                        .disabled(closed),
                ),
            )
            .children(pane.assistant.command_error.as_ref().map(|error| {
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgb(palette.ansi[1]))
                    .child(crate::i18n::localize(error.clone()))
            }))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .h(px(FOOTER_BAR))
                    .border_t_1()
                    .border_color(rgb(palette.border))
                    .px_3()
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(crate::i18n::text("草稿随会话保留 · 支持多行")),
                    )
                    .child(
                        crate::controls::button(format!("send-command-{id}"))
                            .primary()
                            .compact()
                            .icon(Icon::new(IconName::Send).size_4())
                            .label(crate::i18n::text("发送命令"))
                            .disabled(closed)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.send_composed_command(id, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_agent_input(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let session = &pane.assistant.session;
        let closed = session.ended || pane.view.read(cx).snapshot.closed;
        let busy = session.pending.is_some() || session.tool_running();
        let palette = self.palette();
        let font_size = self.store.profile.preferences.agent_font_size;
        let input = pane.assistant.prompt.read(cx);
        let line_height = input.line_height().map_or(font_size * 1.5, f32::from);
        let layout = PromptLayout::new(
            pane.dock().extent(pane.dock_limit()),
            line_height,
            if busy {
                1
            } else {
                input.display_map().wrap_row_count()
            },
            pane.assistant.prompt_expanded && !busy,
        );
        // Let the native editor observe real wrap changes, capped to the dock.
        if pane.assistant.prompt_max_rows.replace(layout.max_rows) != layout.max_rows {
            pane.assistant.prompt.update(cx, |input, cx| {
                input.set_auto_grow(1, layout.max_rows, cx);
            });
        }
        let height = layout.height;
        let expanded = pane.assistant.prompt_expanded;
        div()
            .id(format!("agent-input-{id}"))
            .flex()
            .flex_shrink_0()
            .min_w_0()
            .h(px(height))
            .border_t_1()
            .border_color(rgb(palette.border))
            .on_key_down(
                cx.listener(move |view, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape"
                        && view
                            .panes
                            .get(&id)
                            .is_some_and(|pane| pane.assistant.prompt_expanded)
                    {
                        view.toggle_prompt_expansion(id, window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .overflow_hidden()
                    .cursor_text()
                    .on_mouse_down(
                        gpui_kit::MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            if !closed
                                && !busy
                                && let Some(pane) = view.panes.get(&id)
                            {
                                pane.assistant
                                    .prompt
                                    .update(cx, |input, cx| input.focus(window, cx));
                            }
                        }),
                    )
                    .child(
                        Textarea::new(&pane.assistant.prompt)
                            .appearance(false)
                            .bordered(false)
                            .rounded_none()
                            .font(
                                crate::fonts::FontPair::agent(&self.store.profile.preferences, cx)
                                    .latin,
                            )
                            .text_size(px(font_size))
                            .aria_label(crate::i18n::text("AI 消息"))
                            .flex_1()
                            .min_w_0()
                            .h(px(layout.editor_height))
                            .opacity(if busy { 0. } else { 1. })
                            .disabled(closed || busy),
                    )
                    .children(busy.then(|| {
                        div()
                            .absolute()
                            .left(px(8.))
                            .bottom(px((FOOTER_BAR - 1. - 18.) / 2.))
                            .child(waiting_dots(
                                id,
                                session
                                    .pending
                                    .as_ref()
                                    .map_or(session.id, |pending| pending.id),
                                palette.accent,
                            ))
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .h_full()
                            .items_end()
                            .pb(px(ACTION_INSET))
                            .px_1()
                            .child(
                                icon_button(
                                    format!("expand-agent-input-{id}"),
                                    if expanded {
                                        IconName::Minimize2
                                    } else {
                                        IconName::Maximize2
                                    },
                                    crate::i18n::text(if expanded {
                                        "收起输入框"
                                    } else {
                                        "展开输入框"
                                    }),
                                )
                                .w(px(INPUT_ACTION))
                                .h(px(INPUT_ACTION))
                                .disabled(closed || busy)
                                .on_click(cx.listener(
                                    move |view, _, window, cx| {
                                        view.toggle_prompt_expansion(id, window, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_end()
                    .border_l_1()
                    .border_color(rgb(palette.border))
                    .px_2()
                    .pb(px(ACTION_INSET))
                    .child(self.render_prompt_actions(id, cx)),
            )
            .into_any_element()
    }

    fn render_prompt_actions(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let session = &pane.assistant.session;
        let session_id = session.id;
        if session.pending.is_some() || session.tool_running() {
            crate::controls::button(format!("stop-agent-{id}"))
                .ghost()
                .compact()
                .icon(
                    Icon::new(IconName::Square)
                        .size_4()
                        .text_color(rgb(self.palette().ansi[1])),
                )
                .label(crate::i18n::text("停止"))
                .on_click(cx.listener(move |view, _, _, cx| {
                    if !view
                        .panes
                        .get(&id)
                        .is_some_and(|pane| pane.assistant.session.id == session_id)
                    {
                        return;
                    }
                    view.activate_pane(id, cx);
                    if let Some(pane) = view.panes.get_mut(&id) {
                        pane.assistant.session.stop();
                    }
                    cx.notify();
                }))
                .into_any_element()
        } else {
            crate::controls::button(format!("ask-agent-{id}"))
                .primary()
                .compact()
                .icon(Icon::new(IconName::Send).size_4())
                .label(crate::i18n::text("发送"))
                .disabled(
                    session.ended
                        || pane.view.read(cx).snapshot.closed
                        || session.unresolved_tools(),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    if view
                        .panes
                        .get(&id)
                        .is_some_and(|pane| pane.assistant.session.id == session_id)
                    {
                        view.ask_pane(id, window, cx);
                    }
                }))
                .into_any_element()
        }
    }
}

pub(super) fn waiting_dots(pane: Uuid, request: Uuid, color: u32) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .h(px(18.))
        .px_2()
        .children((0..3).map(|index| {
            div()
                .relative()
                .size(px(5.))
                .rounded_full()
                .bg(rgb(color))
                .with_animation(
                    format!("agent-wait-{pane}-{request}-{index}"),
                    Animation::new(Duration::from_millis(1100))
                        .repeat()
                        .with_max_fps(30.),
                    move |dot, progress| {
                        let phase = (progress + 1. - index as f32 * 0.16).fract();
                        let bounce = if phase < 0.5 {
                            (phase * std::f32::consts::TAU).sin()
                        } else {
                            0.
                        };
                        dot.top(px(-3. * bounce)).opacity(0.4 + 0.6 * bounce)
                    },
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_grows_with_wrapped_rows_and_releases_space_when_text_is_removed() {
        let size = |rows| PromptLayout::new(300., 20., rows, false).height;
        assert_eq!(size(0), FOOTER_BAR);
        assert_eq!(size(1), FOOTER_BAR);
        assert_eq!(size(3), 77.);
        assert_eq!(size(1), FOOTER_BAR);
    }

    #[test]
    fn expansion_and_large_pastes_are_bounded_by_the_pane() {
        for dock in [112., 240., 400.] {
            let full = PromptLayout::new(dock, 20., 1, true);
            let paste = PromptLayout::new(dock, 20., 10000, false);
            assert!(full.height + DOCK_BAR + AGENT_TIPS + 1. <= dock);
            assert_eq!(full.height, dock - DOCK_BAR - AGENT_TIPS - 1.);
            if dock >= 240. {
                assert!(paste.height + CONVERSATION_MIN <= full.height);
            }
            assert!(paste.height <= full.height);
            assert!(paste.max_rows >= 1);
            assert!(paste.max_rows as f32 * 20. <= full.height);
        }
        assert_eq!(PromptLayout::new(240., 20., 1, false).height, FOOTER_BAR);
    }

    #[test]
    fn compact_editor_has_space_to_center_text_and_expanded_editor_fills_the_frame() {
        for line in [14., 18., 24., 32.] {
            let layout = PromptLayout::new(300., line, 1, false);
            assert!(layout.editor_height >= line + INPUT_PADDING);
            let inset = (layout.height - 1. - layout.editor_height) * 0.5;
            assert!(inset >= 0.);
            assert_eq!(
                inset + layout.editor_height * 0.5,
                (layout.height - 1.) * 0.5
            );
            let expanded = PromptLayout::new(300., line, 1, true);
            assert_eq!(expanded.editor_height, expanded.height - 1.);
        }
    }
}
