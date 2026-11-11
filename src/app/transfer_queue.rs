//! Compact per-task controls backed by the persistent SFTP scheduler.
use super::*;
use crate::transfers::{Action, State};
pub(super) fn configuration(profile: &crate::config::Profile) -> crate::transfers::Configuration {
    crate::transfers::Configuration {
        connections: profile.connections.clone(),
        proxies: profile.proxies.clone(),
        concurrency: profile.preferences.transfer_concurrency,
        limit: profile.preferences.transfer_limit_kib * 1024,
    }
}
impl WorkspaceView {
    pub(super) fn render_transfer_queue(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1()
            .children(self.transfer_error.as_ref().map(|error| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.ansi[1]))
                    .child(crate::i18n::localize(error.clone()))
            }))
            .children((!self.transfers.is_empty()).then(|| {
                div().text_size(px(12.)).child(format!(
                    "{} · {}",
                    crate::i18n::text("传输队列"),
                    self.transfers.len()
                ))
            }))
            .children((!self.transfers.is_empty()).then(|| {
                uniform_list(
                    "transfer-queue",
                    self.transfers.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        let p = view.palette();
                        range
                            .map(|index| {
                                let task = &view.transfers[index];
                                let id = task.id;
                                div()
                                    .h(px(76.))
                                    .px_1()
                                    .py_1()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .overflow_hidden()
                                    .border_b_1()
                                    .border_color(rgb(p.border))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                Icon::new(
                                                    if task.direction
                                                        == crate::transfers::Direction::Upload
                                                    {
                                                        IconName::Upload
                                                    } else {
                                                        IconName::Download
                                                    },
                                                )
                                                .size_4(),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .text_size(px(11.))
                                                    .text_ellipsis()
                                                    .child(task.target.clone().unwrap_or_else(|| task.name.clone())),
                                            )
                                            .children((task.state == State::Failed && task.conflict).then(|| {
                                                use gpui_kit::component::menu::PopupMenuItem;
                                                let owner = cx.entity().downgrade();
                                                let replaceable = task.replaceable;
                                                icon_button(format!("conflict-{id}"), IconName::Copy, crate::i18n::text("处理同名冲突"))
                                                    .primary()
                                                    .dropdown_menu(move |mut menu, _, _| {
                                                        for (action, label, enabled) in [
                                                            (Action::KeepBoth, "保留两份", true),
                                                            (Action::Skip, "跳过此项", true),
                                                            (Action::ReplaceFile, "替换现有文件", replaceable),
                                                        ] {
                                                            let owner = owner.clone();
                                                            menu = menu.item(PopupMenuItem::new(crate::i18n::text(label)).disabled(!enabled)
                                                                .on_click(move |_, _, cx| { let _ = owner.update(cx, |view, cx| {
                                                                    if let Err(error) = view.transfer_queue.action(id, action) { view.transfer_error = Some(error.to_string()); }
                                                                    cx.notify();
                                                                }); }));
                                                        }
                                                        menu
                                                    })
                                            }))
                                            .children(task.state.pending().then(|| {
                                                view.transfer_button(
                                                    id,
                                                    Action::Pause,
                                                    IconName::Pause,
                                                    "暂停",
                                                    cx,
                                                )
                                            }))
                                            .children(
                                                matches!(task.state, State::Paused | State::Failed)
                                                    .then(|| {
                                                        view.transfer_button(
                                                            id,
                                                            Action::Resume,
                                                            IconName::Play,
                                                            "继续或重试",
                                                            cx,
                                                        )
                                                    }),
                                            )
                                            .children(
                                                matches!(task.state, State::Paused | State::Failed)
                                                    .then(|| {
                                                        view.transfer_button(
                                                            id,
                                                            Action::Restart,
                                                            IconName::RefreshCw,
                                                            "从头重传",
                                                            cx,
                                                        )
                                                    }),
                                            )
                                            .child(
                                                if matches!(
                                                    task.state,
                                                    State::Complete | State::Canceled | State::Skipped
                                                ) {
                                                    view.transfer_button(
                                                        id,
                                                        Action::Remove,
                                                        IconName::X,
                                                        "移除记录",
                                                        cx,
                                                    )
                                                } else {
                                                    view.transfer_button(
                                                        id,
                                                        Action::Cancel,
                                                        IconName::X,
                                                        "取消并清理分片",
                                                        cx,
                                                    )
                                                },
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.muted))
                                            .text_ellipsis()
                                            .child(format!(
                                                "{} · {} / {} · {}",
                                                task.state.label(),
                                                format_bytes(task.bytes),
                                                format_bytes(task.total),
                                                task.host
                                            )),
                                    )
                                    .children(task.error.as_ref().map(|error| {
                                        div()
                                            .text_size(px(10.))
                                            .text_color(rgb(p.ansi[1]))
                                            .text_ellipsis()
                                            .child(crate::i18n::localize(error.clone()))
                                    }))
                            })
                            .collect()
                    }),
                )
                .h(px((self.transfers.len() * 76).min(228) as f32))
            }))
            .into_any_element()
    }
    fn transfer_button(
        &self,
        id: Uuid,
        action: Action,
        icon: IconName,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        icon_button(
            format!("transfer-{id}-{label}"),
            icon,
            crate::i18n::text(label),
        )
        .on_click(cx.listener(move |view, _, _, cx| {
            if let Err(error) = view.transfer_queue.action(id, action) {
                view.transfer_error = Some(error.to_string());
                cx.notify();
            }
        }))
        .into_any_element()
    }
}
