//! Connection-owned directory bookmarks persisted with the application profile.

use super::{WorkspaceView, icon_button};
use crate::config::FileBookmark;
use crate::remote::Node;
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::input::InputState;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, Window, div, prelude::*, px, rgb,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum FileAction {
    Directory,
    Rename,
    Permissions,
    Remove,
}
pub(super) struct FileActionEditor {
    pub id: Uuid,
    node: Arc<Node>,
    path: String,
    directory: bool,
    action: FileAction,
    input: Entity<InputState>,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileContext {
    node: Uuid,
    target: Option<Uuid>,
}

impl WorkspaceView {
    pub(super) fn open_file_action(
        &mut self,
        index: Option<usize>,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.files_loading || self.file_action.is_some() || !self.files_current() {
            return;
        }
        let Some(node) = self.file_node() else {
            return;
        };
        let file = index.and_then(|i| self.files.get(i));
        if !matches!(action, FileAction::Directory) && file.is_none() {
            return;
        }
        let value = if matches!(action, FileAction::Permissions) {
            "644"
        } else {
            file.map_or("", |f| f.name.as_str())
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value.to_string()));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.file_action = Some(FileActionEditor {
            id: Uuid::new_v4(),
            node,
            path: file
                .map(|f| super::remote_path(&self.files_path, &f.name))
                .unwrap_or_else(|| self.files_path.clone()),
            directory: file.is_some_and(|f| f.directory),
            action,
            input,
            busy: false,
            error: None,
        });
        cx.notify();
    }
    pub(super) fn render_file_action(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.file_action.as_ref().expect("file action is open");
        let p = self.palette();
        let title = match editor.action {
            FileAction::Directory => "新建文件夹",
            FileAction::Rename => "重命名",
            FileAction::Permissions => "权限",
            FileAction::Remove if editor.directory => "删除空目录",
            FileAction::Remove => "删除文件",
        };
        super::modal_container(p)
            .child(
                div()
                    .w(px(500.))
                    .p_6()
                    .rounded_xl()
                    .bg(rgb(p.sidebar))
                    .border_1()
                    .border_color(rgb(p.border))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(div().text_size(px(18.)).child(crate::i18n::text(title)))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(format!("{} · {}", editor.node.connection.name, editor.path)),
                    )
                    .children((!matches!(editor.action, FileAction::Remove)).then(|| {
                        crate::controls::input(&editor.input, cx)
                            .w_full()
                            .disabled(editor.busy)
                    }))
                    .children(matches!(editor.action, FileAction::Remove).then(|| {
                        div().child(crate::i18n::text(
                            "删除后无法恢复，请确认目标。目录仅允许删除空目录。",
                        ))
                    }))
                    .children(
                        editor
                            .error
                            .as_ref()
                            .map(|e| div().text_color(rgb(p.ansi[1])).child(e.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                crate::controls::button("cancel-file-action")
                                    .ghost()
                                    .label(crate::i18n::text("取消"))
                                    .disabled(editor.busy)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.file_action = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                crate::controls::button("confirm-file-action")
                                    .primary()
                                    .label(crate::i18n::text("确认"))
                                    .disabled(editor.busy)
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.submit_file_action(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn submit_file_action(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.file_action else {
            return;
        };
        if editor.busy {
            return;
        }
        let value = editor.input.read(cx).value().to_string();
        let operation = match editor.action {
            FileAction::Directory => {
                crate::remote::FileOperation::Directory(super::remote_path(&editor.path, &value))
            }
            FileAction::Rename => crate::remote::FileOperation::Rename {
                source: editor.path.clone(),
                target: super::remote_path(
                    editor.path.rsplit_once('/').map_or(".", |(p, _)| p),
                    &value,
                ),
            },
            FileAction::Remove => crate::remote::FileOperation::Remove {
                path: editor.path.clone(),
                directory: editor.directory,
            },
            FileAction::Permissions => match u32::from_str_radix(value.trim(), 8) {
                Ok(mode) if mode <= 0o7777 => crate::remote::FileOperation::Permissions {
                    path: editor.path.clone(),
                    mode,
                },
                _ => {
                    editor.error =
                        Some(crate::i18n::text("请输入有效的八进制权限，例如 644 或 755").into());
                    cx.notify();
                    return;
                }
            },
        };
        if matches!(editor.action, FileAction::Directory | FileAction::Rename)
            && (value.is_empty()
                || value == "."
                || value == ".."
                || value.contains(['/', '\\', '\0', '\r', '\n']))
        {
            editor.error = Some(crate::i18n::text("请输入有效的文件名").into());
            cx.notify();
            return;
        }
        let id = editor.id;
        let node = editor.node.clone();
        editor.busy = true;
        editor.error = None;
        let events = self.events.clone();
        self.launch(async move {
            let result = node.file_operation(operation).await;
            let _ = events
                .send(super::BackendEvent::FileActionDone { id, result })
                .await;
        });
        cx.notify();
    }
    pub(super) fn file_node(&self) -> Option<Arc<Node>> {
        if let Some(node) = &self.file_connection {
            return Some(node.clone());
        }
        let pane = self.active_pane.and_then(|id| self.panes.get(&id))?;
        let target = pane.engine.directory_target()?;
        let transport = pane.node.as_ref()?;
        let binding = pane
            .file_binding
            .as_ref()
            .map(|(id, node)| (*id, node.connection.id));
        let selected = follow_connection(&target, transport.connection.id, binding)?;
        if selected == transport.connection.id {
            Some(transport.clone())
        } else {
            pane.file_binding.as_ref().map(|(_, node)| node.clone())
        }
    }
    pub(super) fn file_context(&self) -> Option<FileContext> {
        let node = self.file_node()?;
        let target = if self.file_connection.is_some() {
            None
        } else {
            self.active_pane
                .and_then(|id| self.panes.get(&id))
                .and_then(|p| p.engine.directory_target())
                .map(|t| t.id)
        };
        Some(FileContext {
            node: node.connection.id,
            target,
        })
    }
    pub(super) fn files_current(&self) -> bool {
        self.files_context.is_some() && self.files_context == self.file_context()
    }
    fn bind_file_target(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active_pane else {
            return;
        };
        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let Some(target) = pane.engine.directory_target() else {
            return;
        };
        let Some(node) = self.file_connection.clone() else {
            return;
        };
        let Some(path) = pane.engine.environment().last_observed_directory else {
            self.status = Some(crate::i18n::text("当前目标目录尚未识别").into());
            return;
        };
        if let Some((_, cancel)) = self.file_bind.take() {
            cancel.cancel();
        }
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.file_bind = Some((request, cancel.clone()));
        let events = self.events.clone();
        self.launch(async move {
            let result = tokio::select! { () = cancel.cancelled() => return, result = node.verify_directory(&path) => result };
            let _ = events
                .send(super::BackendEvent::SftpBound {
                    request,
                    pane: id,
                    target: target.id,
                    node,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    pub(super) fn connect_files(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some((_, cancel)) = self.file_bind.take() {
            cancel.cancel();
        }
        if self.shutting_down {
            return;
        }
        if let Some((_, cancel)) = self.file_connect.take() {
            cancel.cancel();
        }
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.file_connect = Some((request, cancel.clone()));
        let registry = self.registry.clone();
        let connections = self.store.profile.connections.clone();
        let proxies = self.store.profile.proxies.clone();
        let events = self.events.clone();
        self.launch(async move {
            let result = tokio::select! {
                () = cancel.cancelled() => Err(anyhow::anyhow!("SFTP 连接已取消")),
                result = async {
                    let node = registry.connect_using(id, &connections, &proxies).await?;
                    node.list(&node.connection.initial_directory).await?;
                    Ok(node)
                } => result,
            };
            let _ = events
                .send(super::BackendEvent::SftpConnected { request, result })
                .await;
        });
        self.tool = super::Tool::Files;
        self.tool_size.open = true;
        cx.notify();
    }
    pub(super) fn render_file_connections(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div().text_size(px(12.)).text_ellipsis().child(
                            self.file_connection
                                .as_ref()
                                .map(|n| n.connection.name.clone())
                                .unwrap_or_else(|| crate::i18n::text("跟随当前终端").into()),
                        ),
                    )
                    .children(self.file_connection.as_ref().map(|_| {
                        icon_button(
                            "close-file-connection",
                            IconName::Unplug,
                            crate::i18n::text("关闭文件连接"),
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            if let Some((_, cancel)) = view.file_connect.take() {
                                cancel.cancel();
                            }
                            if let Some(node) = view.file_connection.take()
                                && node.connection.kind == crate::config::ConnectionKind::Sftp
                            {
                                let registry = view.registry.clone();
                                let id = node.connection.id;
                                let events = view.events.clone();
                                view.launch(async move {
                                    if let Err(error) = registry.disconnect(id).await {
                                        let _ = events
                                            .send(super::BackendEvent::Error(format!("{error:#}")))
                                            .await;
                                    }
                                });
                            }
                            view.reset_files(cx);
                            cx.notify();
                        }))
                    }))
                    .child(
                        crate::controls::button("follow-file-terminal")
                            .ghost()
                            .compact()
                            .label(crate::i18n::text("跟随终端"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                if let Some((_, cancel)) = view.file_connect.take() {
                                    cancel.cancel();
                                }
                                view.file_connection = None;
                                view.reset_files(cx);
                                view.path_input.update(cx, |input, cx| {
                                    input.set_value(view.files_path.clone(), window, cx)
                                });
                                cx.notify();
                            })),
                    ),
            )
            .children(self.file_node().map(|node| {
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .text_ellipsis()
                    .child(format!(
                        "SFTP · {}@{}:{}",
                        node.connection.user, node.connection.host, node.connection.port
                    ))
            }))
            .children(
                self.active_pane
                    .and_then(|id| self.panes.get(&id))
                    .and_then(|pane| pane.engine.directory_target())
                    .map(|target| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_size(px(11.)).text_ellipsis().child(format!(
                                "{} · {}",
                                crate::i18n::text("终端目标"),
                                target.name
                            )))
                            .children(self.file_connection.is_some().then(|| {
                                crate::controls::button("bind-sftp-target")
                                    .ghost()
                                    .compact()
                                    .label(crate::i18n::text(if self.file_bind.is_some() {
                                        "正在验证目标目录…"
                                    } else {
                                        "将此 SFTP 绑定到当前目标"
                                    }))
                                    .disabled(self.file_bind.is_some())
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.bind_file_target(cx)),
                                    )
                            }))
                            .children(self.file_node().is_none().then(|| {
                                div().text_size(px(11.)).text_color(rgb(p.muted)).child(
                                    crate::i18n::text(
                                        "请选择目标主机的 SFTP 连接并绑定，避免操作网关文件",
                                    ),
                                )
                            }))
                    }),
            )
            .children(self.file_connect.as_ref().map(|_| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text("正在连接 SFTP…"))
            }))
            .child(
                div()
                    .id("sftp-connections")
                    .max_h(px(96.))
                    .overflow_y_scroll()
                    .children(self.store.profile.connections.iter().map(|connection| {
                        let id = connection.id;
                        crate::controls::button(format!("open-sftp-{id}"))
                            .ghost()
                            .compact()
                            .icon(gpui_kit::component::Icon::new(IconName::Folder).size_4())
                            .label(connection.name.clone())
                            .on_click(cx.listener(move |view, _, _, cx| view.connect_files(id, cx)))
                    })),
            )
            .into_any_element()
    }
    pub(super) fn render_file_bookmarks(&self, cx: &mut Context<Self>) -> AnyElement {
        let connection = self.file_node().map(|node| node.connection.id);
        let busy = self.configuration_save.is_some() || self.data_busy.is_some();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                crate::controls::button("bookmark-directory")
                    .ghost()
                    .compact()
                    .label(crate::i18n::text("收藏当前目录"))
                    .disabled(busy || self.files_loading)
                    .on_click(cx.listener(|view, _, _, cx| view.bookmark_directory(cx))),
            )
            .child(
                div()
                    .id("file-bookmarks")
                    .max_h(px(120.))
                    .overflow_y_scroll()
                    .children(
                        self.store
                            .profile
                            .file_bookmarks
                            .iter()
                            .filter(|bookmark| Some(bookmark.connection) == connection)
                            .map(|bookmark| {
                                let id = bookmark.id;
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        crate::controls::button(format!("open-bookmark-{id}"))
                                            .ghost()
                                            .compact()
                                            .label(bookmark.path.clone())
                                            .flex_1()
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                let Some(bookmark) = view
                                                    .store
                                                    .profile
                                                    .file_bookmarks
                                                    .iter()
                                                    .find(|bookmark| bookmark.id == id)
                                                else {
                                                    return;
                                                };
                                                if view.file_node().is_none_or(|node| {
                                                    node.connection.id != bookmark.connection
                                                }) {
                                                    return;
                                                }
                                                view.files_path = bookmark.path.clone();
                                                view.path_input.update(cx, |input, cx| {
                                                    input.set_value(
                                                        view.files_path.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                });
                                                view.load_files(cx);
                                            })),
                                    )
                                    .child(
                                        icon_button(
                                            format!("delete-bookmark-{id}"),
                                            IconName::X,
                                            crate::i18n::text("删除书签"),
                                        )
                                        .disabled(busy)
                                        .on_click(
                                            cx.listener(move |view, _, _, cx| {
                                                let mut profile = view.store.profile.clone();
                                                profile
                                                    .file_bookmarks
                                                    .retain(|bookmark| bookmark.id != id);
                                                if let Err(error) = view.persist_profile(
                                                    Uuid::new_v4(),
                                                    profile,
                                                    Vec::new(),
                                                    cx,
                                                ) {
                                                    view.status = Some(error.to_string());
                                                    cx.notify();
                                                }
                                            }),
                                        ),
                                    )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn bookmark_directory(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.file_node() else {
            return;
        };
        let mut profile = self.store.profile.clone();
        if profile.file_bookmarks.iter().any(|bookmark| {
            bookmark.connection == node.connection.id && bookmark.path == self.files_path
        }) {
            return;
        }
        profile.file_bookmarks.push(FileBookmark {
            id: Uuid::new_v4(),
            connection: node.connection.id,
            path: self.files_path.clone(),
        });
        if let Err(error) = self.persist_profile(Uuid::new_v4(), profile, Vec::new(), cx) {
            self.status = Some(error.to_string());
            cx.notify();
        }
    }
}

fn follow_connection(
    target: &crate::terminal::DirectoryTarget,
    transport: Uuid,
    binding: Option<(Uuid, Uuid)>,
) -> Option<Uuid> {
    if let Some((identity, connection)) = binding
        && identity == target.id
    {
        return Some(connection);
    }
    target.transport_bound.then_some(transport)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gateway_target_changes_invalidate_file_binding_even_with_identical_paths() {
        let (terminal, _) = crate::terminal::Terminal::channel(
            &crate::config::Preferences::default(),
            "gateway".into(),
        );
        let environment = nutty_core::session_environment::Environment::from_ssh_probe(
            b"NUTTY_ENV_V1\n/bin/bash\nLinux\n6.8\nx86_64\n",
        )
        .expect("environment");
        terminal.engine.set_environment(environment);
        terminal.engine.set_remote_identity("admin", "gateway");
        let gateway = Uuid::new_v4();
        let target_node = Uuid::new_v4();
        let before = terminal
            .engine
            .directory_target()
            .expect("transport target");
        assert_eq!(follow_connection(&before, gateway, None), Some(gateway));
        terminal.engine.feed(b"\x1b]0;admin@target:/same\x07");
        let target = terminal.engine.directory_target().expect("nested target");
        assert_eq!(follow_connection(&target, gateway, None), None);
        let binding = Some((target.id, target_node));
        assert_eq!(
            follow_connection(&target, gateway, binding),
            Some(target_node)
        );
        terminal.engine.feed(b"\x1b]0;admin@another:/same\x07");
        let another = terminal.engine.directory_target().expect("other target");
        assert_eq!(follow_connection(&another, gateway, binding), None);
        assert_ne!(
            FileContext {
                node: target_node,
                target: Some(target.id)
            },
            FileContext {
                node: target_node,
                target: Some(another.id)
            }
        );
    }
}
