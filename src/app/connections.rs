//! Reusable jump-host drafts, complete route previews and isolated connection probes.

use super::*;
use crate::ssh_options::Category;
use gpui_kit::SharedString;
use gpui_kit::component::checkbox::Checkbox;

pub(super) struct JumpDraft {
    form: Form,
    select_saved: bool,
}

pub(super) struct ConnectionRemoval {
    pub id: Uuid,
    pub error: Option<String>,
}

pub(super) fn profile_without_connection(
    profile: &crate::config::Profile,
    id: Uuid,
) -> Result<crate::config::Profile> {
    ensure!(
        profile
            .connections
            .iter()
            .any(|connection| connection.id == id),
        crate::i18n::text("连接不存在")
    );
    let dependents: Vec<_> = profile
        .connections
        .iter()
        .filter(|connection| connection.jump == Some(id))
        .map(|connection| connection.name.as_str())
        .collect();
    ensure!(
        dependents.is_empty(),
        "{} {}",
        crate::i18n::text("此连接被以下跳板路径引用，请先修改这些连接："),
        dependents.join("、")
    );
    let mut next = profile.clone();
    next.connections.retain(|connection| connection.id != id);
    next.forwards.retain(|rule| rule.connection != id);
    next.validate()?;
    Ok(next)
}

pub(super) struct ConnectionProbe {
    pub id: Uuid,
    cancel: CancellationToken,
}
pub(super) struct AgentProbe {
    pub id: Uuid,
    cancel: CancellationToken,
}
impl Drop for AgentProbe {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Drop for ConnectionProbe {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl WorkspaceView {
    pub(super) fn request_connection_removal(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.configuration_save.is_some()
            || self.data_busy.is_some()
            || self.modal.is_some()
            || self.shutting_down
        {
            return;
        }
        if self
            .store
            .profile
            .connections
            .iter()
            .any(|connection| connection.id == id)
        {
            self.connection_removal = Some(ConnectionRemoval { id, error: None });
            cx.notify();
        }
    }

    fn delete_connection(&mut self, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.shutting_down {
            return;
        }
        let Some(removal) = &self.connection_removal else {
            return;
        };
        let id = removal.id;
        let result = profile_without_connection(&self.store.profile, id).and_then(|profile| {
            self.persist_profile_removing_secrets(id, profile, Vec::new(), vec![id.to_string()], cx)
        });
        if let Err(error) = result
            && let Some(removal) = &mut self.connection_removal
        {
            removal.error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    pub(super) fn render_connection_removal(&self, cx: &mut Context<Self>) -> AnyElement {
        let removal = self
            .connection_removal
            .as_ref()
            .expect("removal dialog exists");
        let p = self.palette();
        let connection = self
            .store
            .profile
            .connections
            .iter()
            .find(|connection| connection.id == removal.id);
        let plan = profile_without_connection(&self.store.profile, removal.id);
        let busy = self.configuration_save.is_some();
        modal_container(p)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .w(px(520.))
                    .max_w(gpui_kit::relative(0.9))
                    .rounded_lg()
                    .bg(rgb(p.sidebar))
                    .border_1()
                    .border_color(rgb(p.border))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::Trash).size_5())
                            .child(crate::i18n::text("删除连接")),
                    )
                    .children(connection.map(|connection| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(connection.name.clone())
                            .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                                format!(
                                    "{}@{}:{}",
                                    connection.user, connection.host, connection.port
                                ),
                            ))
                    }))
                    .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                        crate::i18n::text(
                            "删除保存的连接及其凭据，不关闭已打开的会话，也不删除私钥文件",
                        ),
                    ))
                    .children(plan.as_ref().err().map(|error| {
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0xf17c82))
                            .child(error.to_string())
                    }))
                    .children(removal.error.as_ref().map(|error| {
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(0xf17c82))
                            .child(crate::i18n::localize(error.clone()))
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                crate::controls::button("cancel-connection-removal")
                                    .ghost()
                                    .label(crate::i18n::text("取消"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.connection_removal = None;
                                        view.focus_terminal(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                crate::controls::button("confirm-connection-removal")
                                    .primary()
                                    .icon(Icon::new(IconName::Trash).size_4())
                                    .label(crate::i18n::text(if busy {
                                        "删除中…"
                                    } else {
                                        "删除连接"
                                    }))
                                    .disabled(busy || plan.is_err())
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.delete_connection(cx)),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn open_connection_proxy(
        &mut self,
        editing: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else {
            return;
        };
        let id = form.id;
        self.edit_record(SettingsPage::Proxies, editing, window, cx);
        if let Some(editor) = &self.record_editor {
            self.connection_proxy_editor = Some((id, editor.id));
        }
    }

    pub(super) fn refresh_connection_proxies(
        &mut self,
        preferred: Option<(Uuid, Uuid)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(form) = &mut self.form
            && self.modal == Some(Modal::Connection)
        {
            let current = form
                .proxy
                .as_ref()
                .and_then(|state| state.read(cx).selected_index(cx))
                .and_then(|index| form.proxy_ids.get(index.row))
                .copied()
                .flatten();
            let selected = preferred
                .filter(|(id, _)| *id == form.id)
                .map(|(_, id)| id)
                .or(current);
            refresh_proxy_choices(form, &self.store.profile, selected, window, cx);
        }
        for draft in &mut self.jump_drafts {
            let current = draft
                .form
                .proxy
                .as_ref()
                .and_then(|state| state.read(cx).selected_index(cx))
                .and_then(|index| draft.form.proxy_ids.get(index.row))
                .copied()
                .flatten();
            refresh_proxy_choices(&mut draft.form, &self.store.profile, current, window, cx);
        }
    }
    pub(super) fn connection_form(
        &mut self,
        editing: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some() || self.data_busy.is_some() || self.shutting_down {
            return;
        }
        let saved = editing.and_then(|id| {
            self.store
                .profile
                .connections
                .iter()
                .find(|connection| connection.id == id)
        });
        let mut form = Form::new(
            &[
                ("name", ""),
                ("group", ""),
                ("host", ""),
                ("port", "22"),
                ("user", ""),
                ("key", ""),
                ("certificate", ""),
                ("password", ""),
                ("jump", ""),
                ("keepalive", "30"),
                ("keepalive_max", "3"),
                ("reconnect_attempts", "5"),
                ("connect_timeout", "30"),
                ("initial_directory", "."),
                ("agent_socket", ""),
            ],
            window,
            cx,
        );
        if let Some(connection) = saved {
            for (key, value) in [
                ("name", connection.name.clone()),
                ("group", connection.group.clone()),
                ("host", connection.host.clone()),
                ("port", connection.port.to_string()),
                ("user", connection.user.clone()),
                (
                    "jump",
                    connection.jump.map(|id| id.to_string()).unwrap_or_default(),
                ),
            ] {
                form.fields[key].update(cx, |input, cx| input.set_value(value, window, cx));
            }
            form.auth = match &connection.auth {
                Authentication::Password => 1,
                Authentication::Interactive => 3,
                Authentication::Agent => 0,
                Authentication::Key { path, certificate } => {
                    form.fields["key"].update(cx, |input, cx| {
                        input.set_value(path.to_string_lossy().into_owned(), window, cx)
                    });
                    if let Some(certificate) = certificate {
                        form.fields["certificate"].update(cx, |input, cx| {
                            input.set_value(certificate.to_string_lossy().into_owned(), window, cx)
                        });
                    }
                    2
                }
            };
        }
        if let Some(saved) = saved {
            form.id = saved.id;
            form.connection_kind = saved.kind;
            form.fields["initial_directory"].update(cx, |input, cx| {
                input.set_value(saved.initial_directory.clone(), window, cx)
            });
            form.algorithms = saved.ssh.algorithms.clone();
            form.ssh_reconnect = saved.ssh.reconnect;
            if let Some(socket) = &saved.ssh.agent_socket {
                form.fields["agent_socket"].update(cx, |input, cx| {
                    input.set_value(socket.to_string_lossy().into_owned(), window, cx)
                });
            }
            for (key, value) in [
                ("keepalive", saved.ssh.keepalive_seconds.to_string()),
                (
                    "connect_timeout",
                    saved.ssh.connect_timeout_seconds.to_string(),
                ),
                ("keepalive_max", saved.ssh.keepalive_max.to_string()),
                (
                    "reconnect_attempts",
                    saved.ssh.reconnect_attempts.to_string(),
                ),
            ] {
                form.fields[key].update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
        refresh_agent_keys(
            &mut form,
            &[],
            saved.and_then(|saved| saved.ssh.agent_identity.clone()),
            window,
            cx,
        );
        let selected_proxy = saved.and_then(|saved| saved.proxy);
        refresh_proxy_choices(&mut form, &self.store.profile, selected_proxy, window, cx);
        let current_jump = saved.and_then(|connection| connection.jump);
        refresh_jumps(
            &mut form,
            &self.store.profile,
            &self.jump_drafts,
            current_jump,
            window,
            cx,
        );
        form.editing = editing;
        observe_fields(&form, cx);
        form.fields["name"].update(cx, |input, cx| input.focus(window, cx));
        self.form = Some(form);
        self.modal = Some(Modal::Connection);
        if self.form.as_ref().is_some_and(|form| form.auth == 0) {
            self.probe_ssh_agent(cx);
        }
        cx.notify();
    }

    pub(super) fn save_connection(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.configuration_save.is_some()
            || self.data_busy.is_some()
            || self.shutting_down
            || self
                .form
                .as_ref()
                .is_some_and(|form| form.connection_probe.is_some())
        {
            return;
        }
        let result = self.connection_candidate(cx);
        match result {
            Ok((connection, password)) => {
                let id = connection.id;
                let mut profile = self.store.profile.clone();
                if let Some(existing) = profile.connections.iter_mut().find(|item| item.id == id) {
                    *existing = connection;
                } else {
                    profile.connections.push(connection);
                }
                let secrets = if password.is_empty() {
                    Vec::new()
                } else {
                    vec![(id.to_string(), password)]
                };
                if let Err(error) = self.persist_profile(id, profile, secrets, cx) {
                    self.status = Some(error.to_string());
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(super) fn connection_candidate(
        &self,
        cx: &Context<Self>,
    ) -> Result<(Connection, zeroize::Zeroizing<String>)> {
        let form = self
            .form
            .as_ref()
            .context(crate::i18n::text("连接表单不存在"))?;
        let key = form.value("key", cx);
        let jump = form
            .jump
            .as_ref()
            .and_then(|select| select.read(cx).selected_index(cx))
            .and_then(|index| form.jump_ids.get(index.row).copied())
            .flatten();
        let connection = Connection {
            kind: form.connection_kind,
            initial_directory: form.value("initial_directory", cx),
            id: form.id,
            name: form.value("name", cx),
            group: form.value("group", cx),
            host: form.value("host", cx).trim().into(),
            port: form
                .value("port", cx)
                .parse()
                .context(crate::i18n::text("端口无效"))?,
            user: form.value("user", cx).trim().into(),
            jump,
            ssh: crate::config::SshOptions {
                agent_socket: (!form.value("agent_socket", cx).trim().is_empty())
                    .then(|| PathBuf::from(form.value("agent_socket", cx).trim())),
                agent_identity: form
                    .agent_keys
                    .as_ref()
                    .and_then(|select| select.read(cx).selected_index(cx))
                    .and_then(|index| form.agent_key_ids.get(index.row))
                    .cloned()
                    .flatten(),
                connect_timeout_seconds: form
                    .value("connect_timeout", cx)
                    .parse()
                    .context(crate::i18n::text("连接超时无效"))?,
                algorithms: form.algorithms.clone(),
                keepalive_seconds: form
                    .value("keepalive", cx)
                    .parse()
                    .context(crate::i18n::text("保活间隔无效"))?,
                keepalive_max: form
                    .value("keepalive_max", cx)
                    .parse()
                    .context(crate::i18n::text("保活失败次数无效"))?,
                reconnect: form.ssh_reconnect,
                reconnect_attempts: form
                    .value("reconnect_attempts", cx)
                    .parse()
                    .context(crate::i18n::text("重连次数无效"))?,
            },
            proxy: if jump.is_some() {
                None
            } else {
                form.proxy
                    .as_ref()
                    .and_then(|state| state.read(cx).selected_index(cx))
                    .and_then(|index| form.proxy_ids.get(index.row))
                    .copied()
                    .flatten()
            },
            auth: match form.auth {
                1 => Authentication::Password,
                3 => Authentication::Interactive,
                2 => {
                    ensure!(!key.trim().is_empty(), crate::i18n::text("请选择私钥路径"));
                    Authentication::Key {
                        path: key.into(),
                        certificate: {
                            let path = form.value("certificate", cx);
                            (!path.trim().is_empty()).then(|| PathBuf::from(path))
                        },
                    }
                }
                _ => Authentication::Agent,
            },
        };
        connection.validate()?;
        if let Some(id) = connection.proxy {
            let proxy = self
                .store
                .profile
                .proxies
                .iter()
                .find(|proxy| proxy.id == id)
                .context(crate::i18n::text("SSH 代理不存在"))?;
            ensure!(
                proxy.supports_ssh(),
                crate::i18n::text("SSH 的 HTTPS 代理需配置 SOCKS5 或 HTTP CONNECT 端点")
            );
        }
        ensure!(
            connection.jump != Some(connection.id),
            crate::i18n::text("连接不能将自身作为跳板")
        );
        let password = zeroize::Zeroizing::new(form.value("password", cx));
        Ok((connection, password))
    }
}

fn selected_jump(form: &Form, cx: &Context<WorkspaceView>) -> Option<Uuid> {
    form.jump
        .as_ref()
        .and_then(|state| state.read(cx).selected_index(cx))
        .and_then(|index| form.jump_ids.get(index.row))
        .copied()
        .flatten()
}

fn jump_candidates<'profile>(
    profile: &'profile crate::config::Profile,
    excluded: &[Uuid],
) -> Vec<&'profile Connection> {
    profile
        .connections
        .iter()
        .filter(|connection| {
            crate::remote::route(connection.id, &profile.connections).is_ok_and(|route| {
                route.len() + excluded.len() <= 16
                    && route.iter().all(|hop| !excluded.contains(&hop.id))
            })
        })
        .collect()
}

fn refresh_jumps(
    form: &mut Form,
    profile: &crate::config::Profile,
    drafts: &[JumpDraft],
    selected: Option<Uuid>,
    window: &mut Window,
    cx: &mut Context<WorkspaceView>,
) {
    let mut excluded = vec![form.id];
    excluded.extend(drafts.iter().map(|draft| draft.form.id));
    let candidates = jump_candidates(profile, &excluded);
    let mut labels = vec![gpui_kit::SharedString::from(crate::i18n::text("直接连接"))];
    form.jump_ids = vec![None];
    for connection in candidates {
        form.jump_ids.push(Some(connection.id));
        labels.push(
            format!(
                "{} · {}@{}:{}",
                connection.name, connection.user, connection.host, connection.port
            )
            .into(),
        );
    }
    let index = form
        .jump_ids
        .iter()
        .position(|id| *id == selected)
        .unwrap_or(0);
    let state = cx.new(|cx| {
        crate::controls::select_state(labels, Some(IndexPath::new(index)), window, cx)
            .searchable(true)
    });
    observe_route_selection(&state, form.id, cx);
    form.jump = Some(state);
}

fn section(
    title: &'static str,
    description: &'static str,
    p: crate::theme::Palette,
) -> gpui_kit::Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(rgb(p.border))
        .bg(rgb(p.base))
        .child(div().text_size(px(14.)).child(crate::i18n::text(title)))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(p.muted))
                .child(crate::i18n::text(description)),
        )
}

impl WorkspaceView {
    fn open_jump_draft(
        &mut self,
        editing: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some()
            || self
                .form
                .as_ref()
                .is_some_and(|form| form.connection_probe.is_some())
        {
            return;
        }
        if self.jump_drafts.len() >= 15 {
            self.status = Some(crate::i18n::text("SSH 路由最多允许 16 跳").into());
            return;
        }
        if let Some(form) = self.form.take() {
            self.jump_drafts.push(JumpDraft {
                form,
                select_saved: editing.is_none(),
            });
            self.connection_form(editing, window, cx);
        }
    }

    pub(super) fn restore_jump_draft(
        &mut self,
        saved: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut draft) = self.jump_drafts.pop() {
            let selected = if draft.select_saved && saved.is_some() {
                saved
            } else {
                selected_jump(&draft.form, cx)
            };
            refresh_jumps(
                &mut draft.form,
                &self.store.profile,
                &self.jump_drafts,
                selected,
                window,
                cx,
            );
            draft.form.probe_result = None;
            self.form = Some(draft.form);
            self.modal = Some(Modal::Connection);
            if let Some(form) = &self.form {
                form.fields["name"].update(cx, |input, cx| input.focus(window, cx));
            }
        } else {
            self.form = None;
            self.modal = None;
        }
        cx.notify();
    }

    fn probe_connection(&mut self, cx: &mut Context<Self>) {
        if self.configuration_save.is_some()
            || self
                .form
                .as_ref()
                .is_some_and(|form| form.connection_probe.is_some())
        {
            return;
        }
        let candidate = self
            .connection_candidate(cx)
            .and_then(|(connection, secret)| {
                let mut profile = self.store.profile.clone();
                let id = connection.id;
                if let Some(saved) = profile.connections.iter_mut().find(|item| item.id == id) {
                    *saved = connection;
                } else {
                    profile.connections.push(connection);
                }
                profile.validate()?;
                Ok((profile, id, secret))
            });
        let (profile, id, secret) = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                if let Some(form) = &mut self.form {
                    form.probe_result = Some((false, error.to_string()));
                }
                cx.notify();
                return;
            }
        };
        crate::config::retry_secret_access();
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        if let Some(form) = &mut self.form {
            form.probe_result = None;
            form.connection_probe = Some(ConnectionProbe {
                id: request,
                cancel: cancel.clone(),
            });
        }
        let events = self.events.clone();
        let registry = self.registry.clone();
        self.launch(async move {
            let result = registry
                .probe(id, &profile.connections, &profile.proxies, secret, cancel)
                .await;
            let _ = events
                .send(BackendEvent::ConnectionProbe {
                    form: id,
                    request,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    pub(super) fn probe_ssh_agent(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        if form.auth != 0 || form.agent_probe.is_some() {
            return;
        }
        let id = form.id;
        let value = form.value("agent_socket", cx);
        let socket = (!value.trim().is_empty()).then(|| PathBuf::from(value.trim()));
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        form.agent_result = None;
        form.agent_probe = Some(AgentProbe {
            id: request,
            cancel: cancel.clone(),
        });
        let events = self.events.clone();
        self.launch(async move {
            let result = tokio::select! { () = cancel.cancelled() => return, result = crate::ssh_agent::probe(socket.as_deref()) => result };
            let _ = events.send(BackendEvent::SshAgentProbe { form: id, request, result }).await;
        });
        cx.notify();
    }

    pub(super) fn receive_ssh_agent_probe(
        &mut self,
        id: Uuid,
        request: Uuid,
        result: Result<crate::ssh_agent::Info>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.form else {
            return;
        };
        if form.id != id
            || form.auth != 0
            || form
                .agent_probe
                .as_ref()
                .is_none_or(|probe| probe.id != request)
        {
            return;
        }
        form.agent_probe = None;
        let selected = form
            .agent_keys
            .as_ref()
            .and_then(|select| select.read(cx).selected_index(cx))
            .and_then(|index| form.agent_key_ids.get(index.row))
            .cloned()
            .flatten();
        if let Ok(info) = &result {
            refresh_agent_keys(form, &info.identities, selected, window, cx);
        }
        form.agent_result = Some(result.map_err(|error| format!("{error:#}")));
        cx.notify();
    }

    pub(super) fn render_connection_form(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let testing = form.connection_probe.is_some();
        let saving = self.configuration_save.is_some();
        let p = self.palette();
        let jump = selected_jump(form, cx);
        let route = jump
            .and_then(|id| crate::remote::route(id, &self.store.profile.connections).ok())
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children((!self.jump_drafts.is_empty()).then(|| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.accent))
                    .child(crate::i18n::text("配置完成后返回原连接，草稿会保留"))
            }))
            .child(
                section("主机", "为终端、SFTP 和端口转发配置 SSH 连接", p)
                    .child(
                        div().flex().gap_2().children(
                            [
                                (crate::config::ConnectionKind::Ssh, "SSH 终端"),
                                (crate::config::ConnectionKind::Sftp, "独立 SFTP"),
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(index, (kind, label))| {
                                crate::controls::button(format!("connection-kind-{index}"))
                                    .label(crate::i18n::text(label))
                                    .flex_1()
                                    .when(form.connection_kind == kind, |b| b.primary())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(form) = &mut view.form {
                                            form.connection_kind = kind;
                                            form.probe_result = None;
                                        }
                                        cx.notify();
                                    }))
                            }),
                        ),
                    )
                    .children(
                        (form.connection_kind == crate::config::ConnectionKind::Sftp).then(|| {
                            form.field("initial_directory", crate::i18n::text("初始远程目录"), cx)
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(div().flex_1().min_w_0().child(form.field(
                                "name",
                                crate::i18n::text("名称"),
                                cx,
                            )))
                            .child(div().flex_1().min_w_0().child(form.field(
                                "group",
                                crate::i18n::text("分组"),
                                cx,
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(div().flex_1().min_w_0().child(form.field(
                                "host",
                                crate::i18n::text("主机地址"),
                                cx,
                            )))
                            .child(div().w(px(100.)).child(form.field(
                                "port",
                                crate::i18n::text("端口"),
                                cx,
                            ))),
                    )
                    .child(form.field("user", crate::i18n::text("用户名"), cx)),
            )
            .child(
                section("身份认证", "目标主机和每台跳板机使用各自的认证信息", p)
                    .child(
                        div().flex().gap_1().children(
                            [
                                (1, "密码"),
                                (2, "私钥"),
                                (0, "SSH Agent"),
                                (3, "交互式认证"),
                            ]
                            .into_iter()
                            .map(|(auth, label)| {
                                crate::controls::button(format!("auth-{auth}"))
                                    .flex_1()
                                    .label(crate::i18n::text(label))
                                    .when(form.auth == auth, |button| button.primary())
                                    .when(form.auth != auth, |button| button.ghost())
                                    .disabled(saving || testing)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(form) = &mut view.form {
                                            form.invalidate_probe();
                                            form.agent_probe = None;
                                            form.auth = auth;
                                        }
                                        if auth == 0 { view.probe_ssh_agent(cx); }
                                        cx.notify();
                                    }))
                            }),
                        ),
                    )
                    .children((form.auth == 0).then(|| {
                        div().flex().flex_col().gap_2()
                            .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(crate::i18n::text("使用本机 Agent 中已加载的密钥；无需在此填写私钥或密码")))
                            .child(form.path_field("agent_socket", crate::i18n::text("Agent 套接字 · 留空自动识别"), super::paths::PathKind::File, cx))
                            .child(crate::controls::button("inspect-ssh-agent").ghost().compact()
                                .icon(Icon::new(IconName::RefreshCw).size_4())
                                .label(crate::i18n::text(if form.agent_probe.is_some() { "检测 SSH Agent…" } else { "重新检测 SSH Agent" }))
                                .disabled(form.agent_probe.is_some() || testing || saving)
                                .on_click(cx.listener(|view, _, _, cx| view.probe_ssh_agent(cx))))
                            .children(form.agent_result.as_ref().map(|result| {
                                let (color, text) = match result {
                                    Ok(info) if !info.identities.is_empty() => (p.success, crate::i18n::localize(format!("SSH Agent 可用，已加载 {} 个密钥", info.identities.len()))),
                                    Ok(_) => (p.muted, crate::i18n::text("SSH Agent 已连接，但没有密钥。请先使用 ssh-add 加载密钥").into()),
                                    Err(error) => (0xf17c82, crate::i18n::localize(error.clone())),
                                };
                                div().flex().flex_col().gap_1().text_size(px(12.)).text_color(rgb(color)).child(text)
                                    .children(result.as_ref().ok().map(|info| div().text_color(rgb(p.muted)).text_ellipsis().child(info.socket.display().to_string())))
                            }))
                            .children(form.agent_keys.as_ref().map(|state| crate::controls::select(state, cx).w_full().search_placeholder(crate::i18n::text("搜索 Agent 密钥…"))))
                    }))
                    .children((form.auth == 2).then(|| {
                        form.path_field(
                            "key",
                            crate::i18n::text("私钥路径"),
                            super::paths::PathKind::File,
                            cx,
                        )
                    }))
                    .children((form.auth == 2).then(|| {
                        form.path_field(
                            "certificate",
                            crate::i18n::text("用户证书路径 · 可选"),
                            super::paths::PathKind::File,
                            cx,
                        )
                    }))
                    .children((form.auth == 1 || form.auth == 2).then(|| {
                        form.field(
                            "password",
                            crate::i18n::text(if form.auth == 2 {
                                "私钥口令 · 留空保留"
                            } else {
                                "密码 · 保存在系统钥匙串"
                            }),
                            cx,
                        )
                    }))
                    .children((form.auth == 3).then(|| {
                        div()
                            .flex().flex_col().gap_2()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(crate::i18n::text("连接时按服务器提示输入密码或动态验证码"))
                            .child(crate::i18n::text("选择认证方式不会立即登录。点击测试连接可验证服务器是否支持交互式认证"))
                    })),
            )
            .child(
                section("连接路径", "选择已有连接，或为这条路径添加独立的跳板机", p)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(crate::i18n::text("跳板机"))
                            .child(
                                crate::controls::button("add-jump-host")
                                    .ghost()
                                    .compact()
                                    .icon(Icon::new(IconName::Plus).size_4())
                                    .label(crate::i18n::text("添加跳板机"))
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.open_jump_draft(None, window, cx)
                                    })),
                            ),
                    )
                    .children(form.jump.as_ref().map(|state| {
                        crate::controls::select(state, cx)
                            .w_full()
                            .search_placeholder(crate::i18n::text("搜索跳板连接…"))
                    }))
                    .children(route.iter().enumerate().map(|(index, hop)| {
                        let id = hop.id;
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .bg(rgb(p.surface))
                            .rounded_md()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(p.muted))
                                    .child(format!("{}", index + 1)),
                            )
                            .child(Icon::new(IconName::Network).size_4())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(hop.name.clone())
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(p.muted))
                                            .text_ellipsis()
                                            .child(format!(
                                                "{}@{}:{}",
                                                hop.user, hop.host, hop.port
                                            )),
                                    ),
                            )
                            .child(
                                icon_button(
                                    format!("edit-jump-{id}"),
                                    IconName::SquarePen,
                                    crate::i18n::text("编辑跳板机"),
                                )
                                .on_click(cx.listener(
                                    move |view, _, window, cx| {
                                        view.open_jump_draft(Some(id), window, cx)
                                    },
                                )),
                            )
                    }))
                    .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                        if route.is_empty() {
                            crate::i18n::text("本机 → 目标主机").into()
                        } else {
                            crate::i18n::localize(format!(
                                "本机 → {} 台跳板机 → 目标主机",
                                route.len()
                            ))
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().flex().items_center().justify_between()
                                .child(crate::i18n::text("网络代理 · 第一跳使用"))
                                .child(crate::controls::button("add-connection-proxy").ghost().compact()
                                    .icon(Icon::new(IconName::Plus).size_4()).label(crate::i18n::text("添加代理"))
                                    .disabled(jump.is_some() || testing || saving)
                                    .on_click(cx.listener(|view, _, window, cx| view.open_connection_proxy(None, window, cx)))))
                            .children(form.proxy.as_ref().map(|state| {
                                crate::controls::select(state, cx)
                                    .w_full()
                                    .disabled(jump.is_some())
                            })),
                    )
                    .children((self.store.profile.proxies.is_empty()).then(|| div().text_size(px(12.)).text_color(rgb(p.muted)).child(crate::i18n::text("尚未配置代理。添加 HTTP CONNECT 或 SOCKS5 代理后即可选择"))))
                    .children(form.proxy.as_ref().and_then(|state| state.read(cx).selected_index(cx)).and_then(|index| form.proxy_ids.get(index.row)).copied().flatten().map(|proxy| {
                        crate::controls::button("edit-connection-proxy").ghost().compact().icon(Icon::new(IconName::SquarePen).size_4())
                            .label(crate::i18n::text("编辑所选代理")).disabled(jump.is_some() || testing || saving)
                            .on_click(cx.listener(move |view, _, window, cx| view.open_connection_proxy(Some(proxy), window, cx)))
                    }))
                    .children(jump.is_some().then(|| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(crate::i18n::text(
                                "通过跳板连接时，网络代理使用第一台跳板机的配置",
                            ))
                    })),
            )
            .child(
                section("连接稳定性", "配置握手超时、保活和断线恢复", p)
                    .child(form.field(
                        "connect_timeout",
                        crate::i18n::text("连接超时（秒，1–300）"),
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(div().flex_1().child(form.field(
                                "keepalive",
                                crate::i18n::text("保活间隔（秒，0 关闭）"),
                                cx,
                            )))
                            .child(div().flex_1().child(form.field(
                                "keepalive_max",
                                crate::i18n::text("允许保活失败次数"),
                                cx,
                            ))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(crate::i18n::text("网络中断后自动重连"))
                            .child(
                                gpui_kit::component::switch::Switch::new("ssh-auto-reconnect")
                                    .cursor_pointer()
                                    .checked(form.ssh_reconnect)
                                    .on_click(cx.listener(|view, value: &bool, _, cx| {
                                        if let Some(form) = &mut view.form {
                                            form.ssh_reconnect = *value;
                                            form.invalidate_probe();
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(form.field(
                        "reconnect_attempts",
                        crate::i18n::text("最多重连次数（0–10）"),
                        cx,
                    )),
            )
            .child(self.render_ssh_algorithms(form, cx))
            .into_any_element()
    }

    fn render_ssh_algorithms(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        section("SSH 算法", "默认使用现代算法；按类别选择并调整协商顺序", p)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(Category::ALL.into_iter().map(|category| {
                        crate::controls::button(format!("ssh-algorithm-{}", category.label()))
                            .ghost()
                            .compact()
                            .label(category.label())
                            .when(form.algorithm_category == Some(category), |button| {
                                button.primary()
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if let Some(form) = &mut view.form {
                                    form.algorithm_category =
                                        if form.algorithm_category == Some(category) {
                                            None
                                        } else {
                                            Some(category)
                                        };
                                }
                                cx.notify();
                            }))
                    })),
            )
            .children(form.algorithm_category.map(|category| {
                let selected = form.algorithms.selected(category);
                let mut choices = selected.clone();
                choices.extend(
                    category
                        .supported()
                        .into_iter()
                        .filter(|name| !selected.contains(name)),
                );
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                                crate::i18n::text(if form.algorithms.values(category).is_some() {
                                    "自定义 · 从上到下优先协商"
                                } else {
                                    "系统默认 · 从上到下优先协商"
                                }),
                            ))
                            .child(
                                crate::controls::button("reset-ssh-category")
                                    .ghost()
                                    .compact()
                                    .label(crate::i18n::text("恢复默认"))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if let Some(form) = &mut view.form {
                                            *form.algorithms.values_mut(category) = None;
                                            form.invalidate_probe();
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
                    .children(choices.into_iter().map(|name| {
                        let position = selected.iter().position(|value| value == &name);
                        let checked = position.is_some();
                        let checkbox_name = name.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_1()
                            .child(
                                Checkbox::new(format!("ssh-check-{name}"))
                                    .cursor_pointer()
                                    .checked(checked)
                                    .disabled(checked && selected.len() == 1)
                                    .on_click(cx.listener(move |view, checked: &bool, _, cx| {
                                        if let Some(form) = &mut view.form {
                                            form.invalidate_probe();
                                            let values = form
                                                .algorithms
                                                .values_mut(category)
                                                .get_or_insert_with(|| category.defaults());
                                            if *checked {
                                                if !values.contains(&checkbox_name) {
                                                    values.push(checkbox_name.clone());
                                                }
                                            } else if values.len() > 1 {
                                                values.retain(|value| value != &checkbox_name);
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_ellipsis()
                                    .child(name.clone()),
                            )
                            .children(position.map(|position| {
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(order_button(
                                        category,
                                        &name,
                                        position,
                                        selected.len(),
                                        -1,
                                        cx,
                                    ))
                                    .child(order_button(
                                        category,
                                        &name,
                                        position,
                                        selected.len(),
                                        1,
                                        cx,
                                    ))
                            }))
                    }))
            }))
            .into_any_element()
    }

    pub(super) fn render_connection_footer(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette();
        let testing = form.connection_probe.is_some();
        let saving = self.configuration_save.is_some();
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .px_6()
            .py_3()
            .border_t_1()
            .border_color(rgb(p.border))
            .children(form.probe_result.as_ref().map(|(success, text)| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(if *success { p.success } else { p.ansi[1] }))
                    .child(text.clone())
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        crate::controls::button("cancel-connection")
                            .ghost()
                            .label(crate::i18n::text("取消"))
                            .disabled(saving)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.restore_jump_draft(None, window, cx)
                            })),
                    )
                    .child(
                        crate::controls::button("test-connection")
                            .ghost()
                            .icon(Icon::new(IconName::Plug).size_4())
                            .label(crate::i18n::text(if testing {
                                "取消测试"
                            } else {
                                "测试连接"
                            }))
                            .disabled(saving)
                            .on_click(cx.listener(|view, _, _, cx| {
                                if let Some(form) = &mut view.form
                                    && form.connection_probe.is_some()
                                {
                                    form.connection_probe = None;
                                } else {
                                    view.probe_connection(cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        crate::controls::button("save-connection")
                            .primary()
                            .label(crate::i18n::text("保存连接"))
                            .disabled(testing || saving)
                            .on_click(
                                cx.listener(|view, _, window, cx| view.save_connection(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn order_button(
    category: Category,
    name: &str,
    position: usize,
    length: usize,
    direction: i8,
    cx: &Context<WorkspaceView>,
) -> Button {
    icon_button(
        format!("ssh-order-{name}-{direction}"),
        if direction < 0 {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        },
        crate::i18n::text(if direction < 0 {
            "提高优先级"
        } else {
            "降低优先级"
        }),
    )
    .disabled(if direction < 0 {
        position == 0
    } else {
        position + 1 >= length
    })
    .on_click(cx.listener(move |view, _, _, cx| {
        if let Some(form) = &mut view.form {
            form.invalidate_probe();
            let values = form
                .algorithms
                .values_mut(category)
                .get_or_insert_with(|| category.defaults());
            let next = if direction < 0 {
                position.saturating_sub(1)
            } else {
                position + 1
            };
            if next < values.len() && position < values.len() {
                values.swap(position, next);
            }
        }
        cx.notify();
    }))
}

impl Form {
    fn invalidate_probe(&mut self) {
        self.connection_probe = None;
        self.probe_result = None;
    }
}

fn observe_route_selection(
    state: &Entity<SelectState<crate::controls::Choices>>,
    id: Uuid,
    cx: &mut Context<WorkspaceView>,
) {
    let mut previous = state.read(cx).selected_index(cx);
    cx.observe(state, move |view, state, cx| {
        let selected = state.read(cx).selected_index(cx);
        if previous != selected {
            previous = selected;
            if let Some(form) = &mut view.form
                && form.id == id
            {
                form.invalidate_probe();
            }
        }
        cx.notify();
    })
    .detach();
}

fn observe_fields(form: &Form, cx: &mut Context<WorkspaceView>) {
    let id = form.id;
    for (&key, input) in &form.fields {
        cx.subscribe(input, move |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(form) = &mut view.form
                && form.id == id
            {
                form.invalidate_probe();
                if key == "agent_socket" {
                    form.agent_probe = None;
                    form.agent_result = None;
                }
                cx.notify();
            }
        })
        .detach();
    }
}

fn proxy_choices(profile: &crate::config::Profile) -> (Vec<Option<Uuid>>, Vec<SharedString>) {
    let mut ids = vec![None];
    let mut names = vec![SharedString::from(crate::i18n::text("直连"))];
    for proxy in &profile.proxies {
        ids.push(Some(proxy.id));
        let unsupported = !proxy.supports_ssh();
        names.push(if unsupported {
            format!("{} · {}", proxy.name, crate::i18n::text("仅 AI / MCP")).into()
        } else {
            proxy.name.clone().into()
        });
    }
    (ids, names)
}

fn refresh_proxy_choices(
    form: &mut Form,
    profile: &crate::config::Profile,
    selected: Option<Uuid>,
    window: &mut Window,
    cx: &mut Context<WorkspaceView>,
) {
    let (mut ids, mut names) = proxy_choices(profile);
    // Never silently turn a missing saved proxy into a direct connection.
    if let Some(id) = selected
        && !ids.contains(&Some(id))
    {
        ids.push(Some(id));
        names.push(crate::i18n::text("已配置代理 · 当前不可用").into());
    }
    let index = ids.iter().position(|id| *id == selected).unwrap_or(0);
    form.proxy_ids = ids;
    let state =
        cx.new(|cx| crate::controls::select_state(names, Some(IndexPath::new(index)), window, cx));
    observe_route_selection(&state, form.id, cx);
    form.proxy = Some(state);
}

fn refresh_agent_keys(
    form: &mut Form,
    identities: &[crate::ssh_agent::Identity],
    selected: Option<String>,
    window: &mut Window,
    cx: &mut Context<WorkspaceView>,
) {
    let mut names = vec![SharedString::from(crate::i18n::text("自动选择 Agent 密钥"))];
    form.agent_key_ids = vec![None];
    for identity in identities {
        names.push(format!("{} · {}", identity.label, identity.fingerprint).into());
        form.agent_key_ids.push(Some(identity.fingerprint.clone()));
    }
    if let Some(selected) = &selected
        && !form
            .agent_key_ids
            .iter()
            .any(|id| id.as_ref() == Some(selected))
    {
        names.push(
            format!(
                "{} · {selected}",
                crate::i18n::text("已配置密钥 · 当前未加载")
            )
            .into(),
        );
        form.agent_key_ids.push(Some(selected.clone()));
    }
    let index = form
        .agent_key_ids
        .iter()
        .position(|id| *id == selected)
        .unwrap_or(0);
    let select =
        cx.new(|cx| crate::controls::select_state(names, Some(IndexPath::new(index)), window, cx));
    observe_route_selection(&select, form.id, cx);
    form.agent_keys = Some(select);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deleting_saved_connections_preserves_jump_routes_and_unrelated_configuration() -> Result<()>
    {
        let first = Uuid::new_v4();
        let child = Uuid::new_v4();
        let connection = |id, jump| Connection {
            id,
            jump,
            name: id.to_string(),
            host: "localhost".into(),
            port: 22,
            user: "test".into(),
            group: String::new(),
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            auth: Authentication::Agent,
            ssh: Default::default(),
            proxy: None,
        };
        let profile = crate::config::Profile {
            connections: vec![connection(first, None), connection(child, Some(first))],
            ..Default::default()
        };
        assert!(
            profile_without_connection(&profile, first)
                .expect_err("referenced jump cannot be removed")
                .to_string()
                .contains(&child.to_string())
        );
        assert!(profile_without_connection(&profile, Uuid::new_v4()).is_err());
        let next = profile_without_connection(&profile, child)?;
        assert_eq!(next.connections, vec![profile.connections[0].clone()]);
        assert_eq!(
            serde_json::to_value(&next.preferences)?,
            serde_json::to_value(&profile.preferences)?
        );
        assert!(
            profile_without_connection(&next, first)?
                .connections
                .is_empty()
        );
        assert_eq!(
            profile.connections.len(),
            2,
            "planning must never mutate the active profile"
        );
        Ok(())
    }

    #[test]
    fn proxy_picker_identifies_ssh_incompatible_entries_without_removing_selection() {
        let http = Uuid::new_v4();
        let https = Uuid::new_v4();
        let profile = crate::config::Profile {
            proxies: vec![
                crate::network::Proxy {
                    id: http,
                    name: "http".into(),
                    url: "http://127.0.0.1:8080".into(),
                    username: String::new(),
                },
                crate::network::Proxy {
                    id: https,
                    name: "https".into(),
                    url: "https://127.0.0.1:8443".into(),
                    username: String::new(),
                },
            ],
            ..Default::default()
        };
        let (ids, labels) = proxy_choices(&profile);
        assert_eq!(ids, [None, Some(http), Some(https)]);
        assert!(labels[2].contains("AI / MCP"));
    }
    #[test]
    fn jump_picker_excludes_self_descendants_and_oversized_routes() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();
        let connection = |id, jump| Connection {
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            id,
            name: id.to_string(),
            group: String::new(),
            host: "localhost".into(),
            port: 22,
            user: "test".into(),
            auth: Authentication::Agent,
            jump,
            proxy: None,
            ssh: Default::default(),
        };
        let profile = crate::config::Profile {
            connections: vec![
                connection(first, None),
                connection(second, Some(first)),
                connection(third, None),
            ],
            ..Default::default()
        };
        let choices = jump_candidates(&profile, &[first]);
        assert_eq!(
            choices.iter().map(|choice| choice.id).collect::<Vec<_>>(),
            vec![third]
        );
        assert_eq!(jump_candidates(&profile, &[second]).len(), 2);
        assert!(jump_candidates(&profile, &[Uuid::new_v4(); 16]).is_empty());
    }
}
