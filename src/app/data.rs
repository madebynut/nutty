//! Previewed configuration import and explicit encrypted backup export.

use super::{BackendEvent, WorkspaceView};
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{button::ButtonVariants as _, switch::Switch};
use gpui_kit::{AnyElement, Context, IntoElement, Window, div, prelude::*, px, rgb};
use uuid::Uuid;
use zeroize::Zeroizing;

impl WorkspaceView {
    pub(super) fn render_data_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.data_busy.is_some() || self.configuration_save.is_some();
        let p = self.palette();
        let credentials = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(crate::i18n::text("导出时包含已保存凭据"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(crate::i18n::text("凭据随备份一起加密")),
                    ),
            )
            .child(
                Switch::new("backup-credentials")
                    .cursor_pointer()
                    .color(rgb(p.accent))
                    .accessibility_label(crate::i18n::text("导出时包含已保存凭据"))
                    .checked(self.data_credentials)
                    .disabled(busy)
                    .on_click(cx.listener(|view, value: &bool, _, cx| {
                        view.data_credentials = *value;
                        cx.notify();
                    })),
            );
        let backup = div().flex().flex_col().gap_3()
            .child(crate::i18n::text("备份密码"))
            .child(crate::controls::input(&self.data_password, cx).w_full().disabled(busy))
            .child(credentials)
            .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(crate::i18n::text("备份使用 Argon2id 和 AES-256-GCM。私钥文件、日志和命令历史不包含在备份中。")))
            .child(div().flex().gap_2().flex_wrap()
                .child(crate::controls::button("export-backup").primary().label(crate::i18n::text("导出加密备份…")).disabled(busy).on_click(cx.listener(|view, _, window, cx| view.export_backup(window, cx))))
                .child(crate::controls::button("import-config").label(crate::i18n::text("选择文件并预览…")).disabled(busy).on_click(cx.listener(|view, _, window, cx| view.import_preview(window, cx)))))
            .into_any_element();
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.settings_heading(
                "导入与备份",
                "导入 OpenSSH 配置或 Nutty 加密备份，先预览后应用。默认合并并保留已有配置。",
            ))
            .child(self.settings_group("加密备份", "导入或导出加密备份时使用此密码", backup));
        if let Some(preview) = &self.data_preview {
            let profile = &preview.archive.profile;
            panel = panel.child(
                div()
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .pt_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(crate::i18n::localize(format!(
                        "预览：{} 个连接，{} 个提供方，{} 个代理，{} 个集成，{} 个凭据",
                        profile.connections.len(),
                        profile.ai_providers.len(),
                        profile.proxies.len(),
                        profile.mcp_servers.len() + profile.agents.len(),
                        preview.archive.secrets.len()
                    )))
                    .child(
                        div()
                            .id("import-connection-preview")
                            .max_h(px(140.))
                            .overflow_y_scroll()
                            .children(profile.connections.iter().map(|item| {
                                div().text_size(px(12.)).child(format!(
                                    "{}  ·  {}@{}:{}",
                                    item.name, item.user, item.host, item.port
                                ))
                            })),
                    )
                    .children(preview.warnings.iter().map(|warning| {
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(0xe7b567))
                            .child(crate::i18n::localize(warning.clone()))
                    }))
                    .child(
                        Switch::new("replace-profile")
                            .cursor_pointer()
                            .label(crate::i18n::text("替换全部配置 · 同名凭据也会被覆盖"))
                            .checked(self.data_replace)
                            .disabled(busy)
                            .on_click(cx.listener(|view, value: &bool, _, cx| {
                                view.data_replace = *value;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                crate::controls::button("apply-import")
                                    .primary()
                                    .label(crate::i18n::text("应用预览中的配置"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, _, cx| view.apply_import(cx))),
                            )
                            .child(
                                crate::controls::button("cancel-import")
                                    .ghost()
                                    .label(crate::i18n::text("取消导入"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.data_preview = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        panel.into_any_element()
    }
    fn export_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_busy.is_some() || self.configuration_save.is_some() {
            return;
        }
        let password = Zeroizing::new(self.data_password.read(cx).value().to_string());
        if password.len() < 8 {
            self.status = Some(crate::i18n::text("备份密码至少 8 字节，最多 4096 字节").into());
            cx.notify();
            return;
        }
        let profile = self.store.profile.clone();
        let credentials = self.data_credentials;
        let id = Uuid::new_v4();
        self.data_busy = Some(id);
        self.data_password
            .update(cx, |input, cx| input.set_value("", window, cx));
        let prompt = cx.prompt_for_new_path(
            &self.store.profile.preferences.directory,
            Some("Nutty.nutty"),
        );
        cx.spawn_in(window, async move |view, cx| match prompt.await {
            Ok(Ok(Some(path))) => {
                let _ = view.update(cx, |view, _| {
                    let events = view.events.clone();
                    view.launch(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            crate::portable::export(profile, &password, credentials, &path)
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.into()));
                        let _ = events.send(BackendEvent::DataExported { id, result }).await;
                    });
                });
            }
            result => {
                let _ = view.update(cx, |view, cx| {
                    view.data_busy = None;
                    if let Ok(Err(error)) = result {
                        view.status = Some(error.to_string());
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
    fn import_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data_busy.is_some() || self.configuration_save.is_some() {
            return;
        }
        let password = Zeroizing::new(self.data_password.read(cx).value().to_string());
        self.data_password
            .update(cx, |input, cx| input.set_value("", window, cx));
        let id = Uuid::new_v4();
        self.data_busy = Some(id);
        let prompt = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::i18n::text("选择 OpenSSH 配置或 Nutty 备份").into()),
        });
        cx.spawn_in(window, async move |view, cx| match prompt.await {
            Ok(Ok(Some(paths))) if !paths.is_empty() => {
                let path = paths[0].clone();
                let _ = view.update(cx, |view, _| {
                    let events = view.events.clone();
                    view.launch(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            crate::portable::load(
                                &path,
                                (!password.is_empty()).then_some(password.as_str()),
                            )
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.into()));
                        let _ = events.send(BackendEvent::DataLoaded { id, result }).await;
                    });
                });
            }
            result => {
                let _ = view.update(cx, |view, cx| {
                    view.data_busy = None;
                    if let Ok(Err(error)) = result {
                        view.status = Some(error.to_string());
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
    fn apply_import(&mut self, cx: &mut Context<Self>) {
        if self.data_busy.is_some() || self.configuration_save.is_some() {
            return;
        }
        let Some(preview) = &self.data_preview else {
            return;
        };
        let result = if self.data_replace {
            crate::portable::replace(&self.store.profile, &preview.archive)
        } else {
            crate::portable::merge(&self.store.profile, &preview.archive)
        };
        match result {
            Ok(mut archive) => {
                let id = Uuid::new_v4();
                let profile = std::mem::take(&mut archive.profile);
                let secrets = std::mem::take(&mut archive.secrets)
                    .into_iter()
                    .map(|(name, value)| (name, Zeroizing::new(value)))
                    .collect();
                match self.persist_profile(id, profile, secrets, cx) {
                    Ok(()) => self.data_apply = Some(id),
                    Err(error) => self.status = Some(error.to_string()),
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        }
        cx.notify();
    }
}
