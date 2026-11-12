//! Persistent forwarding rules with stable ownership of asynchronous listener startup.
use super::*;
use nutty_core::forwarding::Rule;

pub(super) struct Running {
    rule: Rule,
    request: Uuid,
    cancel: CancellationToken,
    forward: Option<Forward>,
    error: Option<String>,
}
impl Running {
    fn active(&self) -> bool {
        self.error.is_none()
            && !self.cancel.is_cancelled()
            && self
                .forward
                .as_ref()
                .is_none_or(|forward| !forward.cancel.is_cancelled())
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Amber security notice shown under a setting, for example an exposed listener.
pub(super) fn warning_notice(text: &str) -> gpui_kit::Div {
    div()
        .flex()
        .items_start()
        .gap_2()
        .text_size(px(11.))
        .text_color(rgb(0xe7b567))
        .child(
            Icon::new(IconName::TriangleAlert)
                .size_3()
                .flex_shrink_0()
                .mt(px(2.)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(crate::i18n::text(text).to_owned()),
        )
}

impl WorkspaceView {
    /// Warning for the listen address being typed, before a mode is chosen.
    pub(super) fn forward_bind_warning(&self, cx: &Context<Self>) -> Option<&'static str> {
        let bind: std::net::SocketAddr = self.forward_bind.read(cx).value().trim().parse().ok()?;
        (!bind.ip().is_loopback()).then_some(
            "此监听地址对其他设备开放：本地转发会让同一网络中的设备访问目标，SOCKS5 将成为无需认证的代理。只想本机使用时请填写 127.0.0.1。",
        )
    }

    pub(super) fn forward(&mut self, mode: ForwardMode, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            let connection = self
                .active_pane
                .and_then(|id| self.panes.get(&id))
                .and_then(|pane| match pane.source {
                    crate::workspace::session::Source::Ssh { connection } => Some(connection),
                    _ => None,
                })
                .context(crate::i18n::text("请先连接 SSH 会话"))?;
            let bind = self
                .forward_bind
                .read(cx)
                .value()
                .parse()
                .context(crate::i18n::text("监听地址格式应为 127.0.0.1:8080"))?;
            let target = if mode == ForwardMode::Dynamic {
                None
            } else {
                let value = self.forward_target.read(cx).value();
                let (host, port) = value
                    .rsplit_once(':')
                    .context(crate::i18n::text("目标格式应为 host:port"))?;
                Some((host.trim_matches(['[', ']']).to_string(), port.parse()?))
            };
            let rule = Rule {
                id: Uuid::new_v4(),
                connection,
                mode,
                bind,
                target,
                on_connect: false,
            };
            rule.validate()?;
            ensure!(
                !self
                    .store
                    .profile
                    .forwards
                    .iter()
                    .any(|old| old.connection == connection
                        && old.mode == mode
                        && old.bind == bind
                        && old.target == rule.target),
                "此转发规则已保存"
            );
            let id = rule.id;
            let mut profile = self.store.profile.clone();
            profile.forwards.push(rule);
            self.persist_profile(id, profile, Vec::new(), cx)?;
            self.forward_after_save = Some(id);
            Ok(())
        })();
        if let Err(error) = result {
            self.status = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    pub(super) fn start_forward(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.shutting_down || self.forwards.get(&id).is_some_and(Running::active) {
            return;
        }
        let Some(rule) = self
            .store
            .profile
            .forwards
            .iter()
            .find(|r| r.id == id)
            .cloned()
        else {
            return;
        };
        if self
            .forwards
            .values()
            .any(|running| running.active() && running.rule.same_listener(&rule))
        {
            self.status = Some(crate::i18n::text("此监听地址已有活动转发，请先停止它").into());
            cx.notify();
            return;
        }
        if rule.exposure_warning().is_some() {
            // Audit trail: a listener reachable from other machines is being opened.
            tracing::warn!(rule = %rule.id, mode = ?rule.mode, bind = %rule.bind, "starting forwarding listener on a non-loopback address");
        }
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.forwards.insert(
            id,
            Running {
                rule: rule.clone(),
                request,
                cancel: cancel.clone(),
                forward: None,
                error: None,
            },
        );
        let registry = self.registry.clone();
        let connections = self.store.profile.connections.clone();
        let proxies = self.store.profile.proxies.clone();
        let events = self.events.clone();
        self.launch(async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = async {
                    let node = registry.connect_using(rule.connection, &connections, &proxies).await?;
                    match (rule.mode, rule.target) {
                        (ForwardMode::Remote, Some(target)) => node.forward_remote(rule.bind, target).await,
                        (_, target) => node.forward(rule.bind, target).await,
                    }
                } => result,
            };
            let _ = events.send(BackendEvent::Forward { rule: id, request, result }).await;
        });
        cx.notify();
    }

    pub(super) fn receive_forward(&mut self, id: Uuid, request: Uuid, result: Result<Forward>) {
        let Some(running) = self
            .forwards
            .get_mut(&id)
            .filter(|r| r.request == request && !r.cancel.is_cancelled())
        else {
            return;
        };
        match result {
            Ok(forward) => running.forward = Some(forward),
            Err(error) => running.error = Some(format!("{error:#}")),
        }
    }
    pub(super) fn synchronize_forward_rules(&mut self) {
        self.forwards.retain(|id, running| {
            self.store.profile.forwards.iter().any(|rule| {
                rule.id == *id
                    && rule.connection == running.rule.connection
                    && rule.mode == running.rule.mode
                    && rule.bind == running.rule.bind
                    && rule.target == running.rule.target
            })
        });
    }
    pub(super) fn start_connection_forwards(&mut self, connection: Uuid, cx: &mut Context<Self>) {
        let ids: Vec<_> = self
            .store
            .profile
            .forwards
            .iter()
            .filter(|rule| rule.connection == connection && rule.on_connect)
            .map(|r| r.id)
            .collect();
        for id in ids {
            self.start_forward(id, cx);
        }
    }
    pub(super) fn has_active_forwards(&self) -> bool {
        self.forwards.values().any(Running::active)
    }

    pub(super) fn render_forward_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        div()
            .id("saved-forwards")
            .flex()
            .flex_col()
            .gap_2()
            .min_h_0()
            .overflow_y_scroll()
            .children(self.store.profile.forwards.iter().map(|rule| {
                let id = rule.id;
                let running = self.forwards.get(&id);
                let active = running.is_some_and(Running::active);
                let connecting = active && running.is_some_and(|r| r.forward.is_none());
                let label = match rule.mode {
                    ForwardMode::Local => "本地转发",
                    ForwardMode::Dynamic => "SOCKS5",
                    ForwardMode::Remote => "远程转发",
                };
                let host = self
                    .store
                    .profile
                    .connections
                    .iter()
                    .find(|c| c.id == rule.connection)
                    .map(|c| c.name.as_str())
                    .unwrap_or_default();
                let busy = self.configuration_save.is_some() || self.data_busy.is_some();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Network)
                                    .size_4()
                                    .text_color(rgb(if active { p.success } else { p.muted })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .text_size(px(12.))
                                    .child(format!("{} · {host}", crate::i18n::text(label))),
                            )
                            .child(
                                icon_button(
                                    format!("toggle-forward-{id}"),
                                    if active {
                                        IconName::Square
                                    } else {
                                        IconName::Play
                                    },
                                    crate::i18n::text(if active {
                                        "停止转发"
                                    } else {
                                        "启动转发"
                                    }),
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        if active {
                                            view.forwards.remove(&id);
                                        } else {
                                            view.start_forward(id, cx);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                icon_button(
                                    format!("remove-forward-{id}"),
                                    IconName::Trash,
                                    crate::i18n::text("删除规则"),
                                )
                                .disabled(busy)
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        let mut profile = view.store.profile.clone();
                                        profile.forwards.retain(|r| r.id != id);
                                        if let Err(error) =
                                            view.persist_profile(id, profile, Vec::new(), cx)
                                        {
                                            view.status = Some(error.to_string());
                                        }
                                    },
                                )),
                            ),
                    )
                    .child(div().text_size(px(11.)).text_ellipsis().child(format!(
                            "{} → {}",
                            rule.bind,
                            rule.target
                                .as_ref()
                                .map(|(host, port)| format!("{host}:{port}"))
                                .unwrap_or_else(|| "SOCKS5".into())
                        )))
                    .children(rule.exposure_warning().map(warning_notice))
                    .children(connecting.then(|| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.accent))
                            .child(crate::i18n::text("连接中…"))
                    }))
                    .children(running.and_then(|r| r.error.as_ref()).map(|error| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.ansi[1]))
                            .child(crate::i18n::localize(error.clone()))
                    }))
                    .child(
                        gpui_kit::component::checkbox::Checkbox::new(format!("auto-forward-{id}"))
                            .label(crate::i18n::text("SSH 连接成功后自动启动"))
                            .checked(rule.on_connect)
                            .disabled(busy)
                            .on_click(cx.listener(move |view, enabled, _, cx| {
                                let mut profile = view.store.profile.clone();
                                if let Some(rule) = profile.forwards.iter_mut().find(|r| r.id == id)
                                {
                                    rule.on_connect = *enabled;
                                }
                                if let Err(error) =
                                    view.persist_profile(id, profile, Vec::new(), cx)
                                {
                                    view.status = Some(error.to_string());
                                }
                            })),
                    )
            }))
            .into_any_element()
    }
}
