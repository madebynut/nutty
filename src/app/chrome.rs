//! Pane-owned controls, searchable application commands and guarded application shutdown.

use super::{Tool, WorkspaceView, icon_button, modal_container};
use crate::{assistant_session::Mode, workspace::Axis};
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    button::ButtonVariants as _,
    menu::{DropdownMenu, PopupMenuItem},
    tooltip::Tooltip,
};
use gpui_kit::{AnyElement, Context, IntoElement, Window, div, prelude::*, px, rgb};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Overlay {
    Quit,
    About,
    Shortcuts,
    Palette,
}

impl WorkspaceView {
    pub(super) fn render_empty_workspace(&self) -> AnyElement {
        div()
            .id("empty-workspace")
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_6()
            .child(
                gpui_kit::img(crate::branding::WORKSPACE_LOGO)
                    .id("empty-workspace-logo")
                    .size(px(128.))
                    .flex_shrink_0()
                    .opacity(0.4)
                    .object_fit(gpui_kit::ObjectFit::Contain),
            )
            .into_any_element()
    }

    pub(crate) fn menu_action(
        &mut self,
        action: &dyn gpui_kit::Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shutting_down {
            return;
        }
        let action = action.as_any();
        if action.is::<crate::Quit>() {
            self.request_quit(window, cx);
        } else if action.is::<crate::About>() {
            self.show_overlay(Overlay::About, window, cx);
        } else if action.is::<crate::CommandPalette>() {
            self.show_overlay(Overlay::Palette, window, cx);
        } else if action.is::<crate::KeyboardShortcuts>() {
            self.show_overlay(Overlay::Shortcuts, window, cx);
        } else if action.is::<crate::NewTerminal>() {
            self.local(None, window, cx);
        } else if action.is::<crate::OpenSettings>() {
            self.settings_form(window, cx);
        } else if action.is::<crate::SplitRight>() {
            self.split(Axis::Horizontal, window, cx);
        } else if action.is::<crate::SplitDown>() {
            self.split(Axis::Vertical, window, cx);
        } else if action.is::<crate::ClosePane>() {
            if let Some(id) = self.active_pane {
                self.close_pane(id, window, cx);
            }
        } else if action.is::<crate::ToggleSidebar>() {
            self.sidebar.toggle();
            cx.notify();
        } else if action.is::<crate::OpenFiles>() {
            self.switch_tool(Tool::Files, cx);
        } else if action.is::<crate::OpenForwarding>() {
            self.switch_tool(Tool::Forwarding, cx);
        } else if let Some(id) = self.active_pane {
            if action.is::<crate::Reconnect>() {
                self.reconnect_pane(id, cx);
            } else if action.is::<crate::ShellMode>() {
                self.set_pane_mode(id, Mode::Shell, window, cx);
            } else if action.is::<crate::AgentMode>() {
                self.set_pane_mode(id, Mode::Agent, window, cx);
            } else if action.is::<crate::SearchTerminal>() {
                self.set_pane_mode(id, Mode::Shell, window, cx);
                if let Some(pane) = self.panes.get(&id) {
                    pane.view
                        .update(cx, |terminal, cx| terminal.open_search(window, cx));
                }
            } else if action.is::<crate::ClearTerminal>()
                && let Some(pane) = self.panes.get(&id)
                && let Err(error) = pane.view.read(cx).terminal.engine.clear_screen()
            {
                self.status = Some(error.to_string());
            }
        }
    }
    pub(super) fn reconnect_pane(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.activate_pane(id, cx);
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let source = pane.source.clone();
        if pane.reconnect.is_some() {
            return;
        }

        // This entry point is a manual reconnect, not the background retry loop.
        crate::config::retry_secret_access();
        let request = Uuid::new_v4();
        let cancel = tokio_util::sync::CancellationToken::new();
        pane.reconnect = Some((request, cancel.clone()));
        pane.assistant.session.stop();
        let registry = self.registry.clone();
        let connections = self.store.profile.connections.clone();
        let proxies = self.store.profile.proxies.clone();
        let preferences = self.store.profile.preferences.clone();
        let events = self.events.clone();
        self.launch(async move {
            let result=tokio::select! {
                ()=cancel.cancelled()=>return,
                result=async {
                    match source {
                        crate::workspace::session::Source::Ssh { connection } => {
                            let node=registry.connect_using(connection,&connections,&proxies).await?;
                            let terminal=node.terminal(&preferences).await?;
                            Ok::<_,anyhow::Error>((Some(node),terminal))
                        }
                        crate::workspace::session::Source::Local { shell, directory } => {
                            let terminal = tokio::task::spawn_blocking(move || {
                                let mut preferences = preferences; preferences.shell = shell; preferences.directory = directory;
                                crate::terminal::Terminal::local(&preferences)
                            }).await??;
                            Ok((None,terminal))
                        }
                    }
                }=>result
            };
            let _=events.send(super::BackendEvent::Reconnected{pane:id,request,result}).await;
        });
        cx.notify();
    }
    pub(super) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::debug!(panes = self.panes.len(), "application quit requested");
        if self.shutting_down {
            return;
        }
        if self.configuration_save.is_none()
            && self.data_busy.is_none()
            && self.connecting.is_empty()
            && !self.has_active_forwards()
            && self.file_connect.is_none()
            && !self.transfers.iter().any(|t| t.state.pending())
            && self
                .file_connection
                .as_ref()
                .is_none_or(|node| node.cancel.is_cancelled())
            && self
                .panes
                .values()
                .all(|pane| pane.view.read(cx).snapshot.closed)
        {
            self.shutdown(cx);
            return;
        }
        self.show_overlay(Overlay::Quit, window, cx);
    }

    fn shutdown(&mut self, cx: &mut Context<Self>) {
        if self.shutting_down || self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        self.checkpoint_workspace();
        self.shutting_down = true;
        self.connecting.clear();
        self.forwards.clear();
        if let Some(cancel) = self.file_listing_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.file_preview_cancel.take() {
            cancel.cancel();
        }
        if let Some((_, cancel)) = self.file_bind.take() {
            cancel.cancel();
        }
        if let Some((_, cancel)) = self.file_connect.take() {
            cancel.cancel();
        }
        if let Some(node) = self.file_connection.take() {
            node.cancel.cancel();
        }
        let engines: Vec<_> = self
            .panes
            .values_mut()
            .map(|pane| {
                pane.assistant.session.end();
                if let Some((_, cancel)) = &pane.reconnect {
                    cancel.cancel();
                }
                if let Some(node) = &pane.node {
                    node.cancel.cancel();
                }
                let engine = pane.view.read(cx).terminal.engine.clone();
                engine.close_input();
                engine
            })
            .collect();
        let workspace_writer = self.workspace_writer.clone();
        let transfer_queue = self.transfer_queue.clone();
        let events = self.events.clone();
        self.launch(async move {
            if let Err(error) = workspace_writer.flush().await {
                let _ = events
                    .send(super::BackendEvent::ShutdownFailed(format!(
                        "工作区保存失败：{error:#}"
                    )))
                    .await;
                return;
            }
            transfer_queue.shutdown().await;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            while engines.iter().any(|engine| !engine.is_closed())
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let result = tokio::task::spawn_blocking(crate::journal::flush).await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!("journal flush did not complete before shutdown");
            }
            let _ = events.send(super::BackendEvent::ShutdownReady).await;
        });
        cx.notify();
    }

    pub(super) fn show_overlay(
        &mut self,
        overlay: Overlay,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay = Some(overlay);
        self.palette_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn render_pane_header(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let snapshot = &pane.view.read(cx).snapshot;
        let p = self.palette();
        let active = Some(id) == self.active_pane;
        let width = f32::from(pane.bounds.get().size.width);
        let narrow = width > 0. && width < 420.;
        let compact = snapshot.columns < 75;
        let wing = if compact { 132. } else { 192. };
        let full_title = snapshot.title.clone();
        let title = full_title.clone();
        let tooltip = snapshot
            .error
            .as_ref()
            .map(|error| format!("{full_title}\n{}", crate::i18n::localize(error.clone())))
            .unwrap_or(full_title);
        let directory = div()
            .id(format!("pane-directory-{id}"))
            .flex()
            .items_center()
            .justify_center()
            .min_w_0()
            .flex_1()
            .px_2()
            .gap_2()
            .overflow_hidden()
            .text_size(px(14.))
            .font_weight(gpui_kit::FontWeight::MEDIUM)
            .child(div().flex_shrink_0().size(px(5.)).rounded_full().bg(rgb(
                if snapshot.error.is_some() {
                    0xf17c82
                } else if snapshot.closed {
                    p.muted
                } else {
                    p.success
                },
            )))
            .child(div().min_w_0().text_ellipsis().child(title))
            .tooltip(move |_, cx| cx.new(|_| Tooltip::new(tooltip.clone())).into());
        let header = div()
            .id(format!("pane-header-{id}"))
            .flex()
            .flex_shrink_0()
            .min_w_0()
            .border_b_1()
            .border_color(rgb(p.border))
            .bg(rgb(if active { p.base } else { p.sidebar }))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(move |view, _, window, cx| {
                    view.activate_pane(id, cx);
                    view.focus_terminal(window, cx);
                    cx.notify();
                }),
            );
        if narrow {
            header
                .flex_col()
                .child(div().flex().h(px(28.)).child(directory))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .h(px(34.))
                        .px_1()
                        .child(self.render_pane_actions(id, cx))
                        .child(self.render_pane_controls(id, cx)),
                )
                .into_any_element()
        } else {
            header
                .items_center()
                .h(px(44.))
                .px_2()
                .child(
                    div()
                        .w(px(wing))
                        .flex_shrink_0()
                        .child(self.render_pane_actions(id, cx)),
                )
                .child(directory)
                .child(
                    div()
                        .w(px(wing))
                        .flex()
                        .justify_end()
                        .flex_shrink_0()
                        .child(self.render_pane_controls(id, cx)),
                )
                .into_any_element()
        }
    }

    fn render_pane_actions(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let remote = matches!(pane.source, crate::workspace::session::Source::Ssh { .. });
        let closed = pane.view.read(cx).snapshot.closed;
        let workspace = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .children(
                [
                    (
                        Axis::Horizontal,
                        "左右分屏",
                        IconName::SquareSplitHorizontal,
                    ),
                    (Axis::Vertical, "上下分屏", IconName::SquareSplitVertical),
                ]
                .into_iter()
                .map(|(axis, label, icon)| {
                    icon_button(
                        format!("pane-split-{id}-{label}"),
                        icon,
                        crate::i18n::text(label),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.activate_pane(id, cx);
                        view.split(axis, window, cx);
                    }))
                }),
            )
            .children(
                pane.view
                    .read(cx)
                    .snapshot
                    .transfer
                    .as_ref()
                    .map(|transfer| {
                        let label = if transfer.file.is_empty() {
                            crate::i18n::text("选择文件…").to_string()
                        } else {
                            format!(
                                "{} · {} / {}",
                                transfer.file,
                                transfer.bytes,
                                transfer
                                    .total
                                    .map(|n| n.to_string())
                                    .unwrap_or_else(|| "?".into())
                            )
                        };
                        icon_button(
                            format!("cancel-zmodem-{}", transfer.id),
                            if transfer.direction == crate::terminal::zmodem::Direction::Send {
                                IconName::Upload
                            } else {
                                IconName::Download
                            },
                            &format!("Zmodem · {label} · {}", crate::i18n::text("取消传输")),
                        )
                        .primary()
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(pane) = view.panes.get(&id) {
                                pane.view.read(cx).terminal.engine.cancel_zmodem();
                            }
                            cx.notify();
                        }))
                    }),
            )
            .child(
                icon_button(
                    format!("pane-broadcast-{id}"),
                    IconName::RadioTower,
                    crate::i18n::text("广播到当前标签的终端"),
                )
                .when(self.broadcast, |button| button.primary())
                .disabled(closed)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.activate_pane(id, cx);
                    view.broadcast = !view.broadcast;
                    cx.notify();
                })),
            )
            .children((closed || pane.reconnect.is_some()).then(|| {
                icon_button(
                    format!("pane-reopen-{id}"),
                    IconName::RefreshCw,
                    crate::i18n::text(if pane.reconnect.is_some() {
                        "连接中…"
                    } else {
                        "重新连接"
                    }),
                )
                .primary()
                .disabled(pane.reconnect.is_some())
                .on_click(cx.listener(move |view, _, _, cx| view.reconnect_pane(id, cx)))
            }))
            .child(
                icon_button(
                    format!("pane-clear-{id}"),
                    IconName::Eraser,
                    crate::i18n::text("清屏"),
                )
                .disabled(closed)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.activate_pane(id, cx);
                    if let Some(pane) = view.panes.get(&id)
                        && let Err(error) = pane.view.read(cx).terminal.engine.clear_screen()
                    {
                        view.status = Some(error.to_string());
                    }
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    format!("pane-actions-{id}"),
                    IconName::Ellipsis,
                    crate::i18n::text("会话操作"),
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for (index, label, icon) in [
                        (1, "远程文件", IconName::FolderOpen),
                        (2, "端口转发", IconName::Network),
                        (3, "重新连接", IconName::RefreshCw),
                        (4, "断开共享 SSH 节点", IconName::Unplug),
                    ] {
                        let workspace = workspace.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::i18n::text(label))
                                .icon(icon)
                                .disabled(!remote && index != 3)
                                .on_click(move |_, _, cx| {
                                    let _ = workspace.update(cx, |view, cx| {
                                        view.activate_pane(id, cx);
                                        match index {
                                            1 => view.switch_tool(Tool::Files, cx),
                                            2 => view.switch_tool(Tool::Forwarding, cx),
                                            3 => view.reconnect_pane(id, cx),
                                            4 => view.disconnect(cx),
                                            _ => {}
                                        }
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                }),
            )
            .into_any_element()
    }

    fn render_pane_controls(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .child(self.render_mode_switch(id, cx))
            .child(
                icon_button(
                    format!("close-pane-{id}"),
                    IconName::X,
                    crate::i18n::text("关闭窗格"),
                )
                .on_click(cx.listener(move |view, _, window, cx| view.close_pane(id, window, cx))),
            )
            .into_any_element()
    }

    pub(super) fn render_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let overlay = self.overlay?;
        let p = self.palette();
        let title = match overlay {
            Overlay::Quit => crate::i18n::text("退出 Nutty？"),
            Overlay::About => crate::i18n::text("关于 Nutty"),
            Overlay::Shortcuts => crate::i18n::text("键盘快捷键"),
            Overlay::Palette => crate::i18n::text("命令面板"),
        };
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .w(px(540.))
            .max_h(px(720.))
            .rounded_lg()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.sidebar))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_size(px(18.)).child(title))
                    .child(
                        icon_button("close-overlay", IconName::X, crate::i18n::text("关闭"))
                            .disabled(self.configuration_save.is_some() || self.data_busy.is_some())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.overlay = None;
                                view.focus_terminal(window, cx);
                                cx.notify();
                            })),
                    ),
            );
        panel = match overlay {
            Overlay::Quit => panel
                .child(crate::i18n::localize(format!(
                    "仍有 {} 个终端会话和 {} 个连接任务。退出将结束终端、AI 会话及传输任务。",
                    self.panes
                        .values()
                        .filter(|pane| !pane.view.read(cx).snapshot.closed)
                        .count(),
                    self.connecting.len()
                )))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            crate::controls::button("cancel-quit")
                                .ghost()
                                .label(crate::i18n::text("继续工作"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.overlay = None;
                                    view.focus_terminal(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            crate::controls::button("confirm-quit")
                                .primary()
                                .label(crate::i18n::text("结束会话并退出"))
                                .disabled(
                                    self.configuration_save.is_some()
                                        || self.data_busy.is_some()
                                        || self.shutting_down,
                                )
                                .on_click(cx.listener(|view, _, _, cx| view.shutdown(cx))),
                        ),
                ),
            Overlay::About => panel
                .child(div().text_size(px(30.)).child("Nutty"))
                .child(crate::i18n::localize(format!(
                    "{} · 原生终端工作区",
                    env!("CARGO_PKG_VERSION")
                )))
                .children(self.update.as_ref().map(|release| {
                    let url = release.url.clone();
                    crate::controls::button("about-update")
                        .primary()
                        .label(crate::i18n::localize(format!(
                            "查看新版本 {}",
                            release.version
                        )))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
                .child("Rust · GPUI Kit · Alacritty · russh · rig")
                .child(crate::i18n::text("独立设计与实现 · Apache License 2.0"))
                .child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(
                    crate::i18n::localize(format!("配置目录：{}", self.store.directory.display())),
                )),
            Overlay::Shortcuts => panel.children(
                [
                    (crate::i18n::text("新建终端"), "⌘T / Ctrl+Shift+T"),
                    (crate::i18n::text("关闭窗格"), "⌘W / Ctrl+Shift+W"),
                    (crate::i18n::text("左右分屏"), "⌘D / Ctrl+Shift+D"),
                    (crate::i18n::text("上下分屏"), "⇧⌘D / Ctrl+Shift+Alt+D"),
                    (crate::i18n::text("命令面板"), "⌘K / Ctrl+Shift+K"),
                    (crate::i18n::text("设置"), "⌘, / Ctrl+Shift+,"),
                    (crate::i18n::text("查找终端"), "⌘F / Ctrl+Shift+F"),
                    (
                        crate::i18n::text("复制 / 粘贴"),
                        "⌘C / ⌘V · Ctrl+Shift+C / V",
                    ),
                    (crate::i18n::text("退出"), "⌘Q / Ctrl+Shift+Q"),
                ]
                .into_iter()
                .map(|(label, key)| {
                    div()
                        .flex()
                        .justify_between()
                        .child(label)
                        .child(div().text_color(rgb(p.muted)).child(key))
                }),
            ),
            Overlay::Palette => {
                let query = self.palette_input.read(cx).value().to_lowercase();
                panel
                    .child(crate::controls::input(&self.palette_input, cx).w_full())
                    .child(
                        div()
                            .id("palette-commands")
                            .flex()
                            .flex_col()
                            .gap_2()
                            .overflow_y_scroll()
                            .children(
                                [
                                    (crate::i18n::text("新建本地终端"), 0),
                                    (crate::i18n::text("左右分屏"), 1),
                                    (crate::i18n::text("上下分屏"), 2),
                                    (crate::i18n::text("Shell 模式"), 3),
                                    (crate::i18n::text("Agent 模式"), 4),
                                    (crate::i18n::text("远程文件"), 5),
                                    (crate::i18n::text("端口转发"), 6),
                                    (crate::i18n::text("重新连接"), 7),
                                    (crate::i18n::text("设置"), 8),
                                    (crate::i18n::text("关闭当前窗格"), 9),
                                    (crate::i18n::text("退出 Nutty"), 10),
                                ]
                                .into_iter()
                                .filter(|(label, _)| label.to_lowercase().contains(&query))
                                .map(|(label, command)| {
                                    crate::controls::button(format!("palette-command-{command}"))
                                        .ghost()
                                        .label(label)
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.overlay = None;
                                            match command {
                                                0 => view.local(None, window, cx),
                                                1 => view.split(Axis::Horizontal, window, cx),
                                                2 => view.split(Axis::Vertical, window, cx),
                                                3 | 4 => {
                                                    if let Some(id) = view.active_pane {
                                                        view.set_pane_mode(
                                                            id,
                                                            if command == 3 {
                                                                Mode::Shell
                                                            } else {
                                                                Mode::Agent
                                                            },
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }
                                                5 => view.switch_tool(Tool::Files, cx),
                                                6 => view.switch_tool(Tool::Forwarding, cx),
                                                7 => {
                                                    if let Some(id) = view.active_pane {
                                                        view.reconnect_pane(id, cx);
                                                    }
                                                }
                                                8 => view.settings_form(window, cx),
                                                9 => {
                                                    if let Some(id) = view.active_pane {
                                                        view.close_pane(id, window, cx);
                                                    }
                                                }
                                                _ => view.request_quit(window, cx),
                                            }
                                            cx.notify();
                                        }))
                                }),
                            ),
                    )
            }
        };
        Some(modal_container(p).child(panel).into_any_element())
    }
}
