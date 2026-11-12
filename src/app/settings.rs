//! Native settings pages and protected provider and proxy configuration.

use super::{BackendEvent, Form, WorkspaceView, icon_button, modal_container};
use crate::{
    config::{Appearance, ClipboardWrite, CursorShape, Profile, TerminalOptions},
    network::Proxy,
    provider::{Kind, Model, Provider},
};
use anyhow::{Context as _, Result};
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    IndexPath,
    button::ButtonVariants as _,
    select::{SelectEvent, SelectState},
    switch::Switch,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, SharedString, Subscription, Window,
    div, prelude::*, px, rgb,
};
use uuid::Uuid;

pub(super) struct ShellEditor {
    state: Entity<SelectState<crate::controls::Choices>>,
    paths: Vec<std::path::PathBuf>,
}

impl ShellEditor {
    pub fn new(
        current: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<WorkspaceView>,
    ) -> Self {
        let paths = crate::shells::installed(current);
        let selected = paths
            .iter()
            .position(|path| path == current)
            .map(IndexPath::new);
        let items = paths
            .iter()
            .map(|path| {
                SharedString::from(format!(
                    "{} · {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    path.display()
                ))
            })
            .collect::<Vec<_>>();
        let state = cx
            .new(|cx| crate::controls::select_state(items, selected, window, cx).searchable(true));
        Self { state, paths }
    }

    pub fn selected(&self, cx: &Context<WorkspaceView>) -> Result<std::path::PathBuf> {
        self.state
            .read(cx)
            .selected_index(cx)
            .and_then(|index| self.paths.get(index.row))
            .cloned()
            .context(crate::i18n::text("请选择本地 Shell"))
    }
}

pub(super) struct ThemeEditor {
    state: Entity<SelectState<crate::controls::Choices>>,
    _subscription: Subscription,
}

impl ThemeEditor {
    pub fn new(current: Appearance, window: &mut Window, cx: &mut Context<WorkspaceView>) -> Self {
        let state = cx.new(|cx| {
            crate::controls::select_state(
                Appearance::ALL
                    .into_iter()
                    .map(|theme| SharedString::from(theme.label()))
                    .collect(),
                Appearance::ALL
                    .iter()
                    .position(|theme| *theme == current)
                    .map(IndexPath::new),
                window,
                cx,
            )
            .searchable(true)
        });
        let subscription = cx.subscribe_in(
            &state,
            window,
            |view, _, event: &SelectEvent<crate::controls::Choices>, window, cx| {
                let SelectEvent::Confirm(Some(label)) = event else {
                    return;
                };
                let Some(appearance) = Appearance::ALL
                    .into_iter()
                    .find(|theme| theme.label() == label.as_ref())
                else {
                    return;
                };
                if let Some(form) = &mut view.form {
                    form.appearance = Some(appearance);
                }
                view.sync_component_theme(cx);
                let mut preferences = view.store.profile.preferences.clone();
                preferences.appearance = appearance;
                for pane in view.panes.values() {
                    pane.view.update(cx, |terminal, cx| {
                        terminal.apply_preferences(&preferences, window, cx)
                    });
                }
                cx.notify();
            },
        );
        Self {
            state,
            _subscription: subscription,
        }
    }
}

pub(super) struct TerminalEditor {
    cursor: Entity<SelectState<crate::controls::Choices>>,
    clipboard: Entity<SelectState<crate::controls::Choices>>,
}

impl TerminalEditor {
    pub fn new(
        options: &TerminalOptions,
        window: &mut Window,
        cx: &mut Context<WorkspaceView>,
    ) -> Self {
        let cursor = cx.new(|cx| {
            crate::controls::select_state(
                ["方块", "竖线", "下划线"]
                    .map(|name| SharedString::from(crate::i18n::text(name)))
                    .to_vec(),
                Some(IndexPath::new(match options.cursor_shape {
                    CursorShape::Block => 0,
                    CursorShape::Beam => 1,
                    CursorShape::Underline => 2,
                })),
                window,
                cx,
            )
        });
        let clipboard = cx.new(|cx| {
            crate::controls::select_state(
                ClipboardWrite::ALL
                    .map(|policy| SharedString::from(policy.label()))
                    .to_vec(),
                ClipboardWrite::ALL
                    .iter()
                    .position(|policy| *policy == options.clipboard_write)
                    .map(IndexPath::new),
                window,
                cx,
            )
        });
        Self { cursor, clipboard }
    }

    pub fn apply(
        &self,
        options: &mut TerminalOptions,
        form: &Form,
        cx: &Context<WorkspaceView>,
    ) -> Result<()> {
        options.cursor_shape = match self
            .cursor
            .read(cx)
            .selected_index(cx)
            .map(|index| index.row)
        {
            Some(0) => CursorShape::Block,
            Some(2) => CursorShape::Underline,
            _ => CursorShape::Beam,
        };
        if let Some(policy) = self
            .clipboard
            .read(cx)
            .selected_index(cx)
            .and_then(|index| ClipboardWrite::ALL.get(index.row))
        {
            options.clipboard_write = *policy;
        }
        options.line_height = form.value("line_height", cx).parse()?;
        options.letter_spacing = form.value("letter_spacing", cx).parse()?;
        options.padding_horizontal = form.value("padding_horizontal", cx).parse()?;
        options.padding_vertical = form.value("padding_vertical", cx).parse()?;
        options.cursor_width = form.value("cursor_width", cx).parse()?;
        options.cursor_blink_ms = form.value("cursor_blink_ms", cx).parse()?;
        options.scroll_speed = form.value("scroll_speed", cx).parse()?;
        options.validate()
    }
}

pub(super) struct FontEditor {
    ui_latin: Entity<SelectState<crate::controls::Choices>>,
    ui_chinese: Entity<SelectState<crate::controls::Choices>>,
    terminal_latin: Entity<SelectState<crate::controls::Choices>>,
    terminal_chinese: Entity<SelectState<crate::controls::Choices>>,
    agent_latin: Entity<SelectState<crate::controls::Choices>>,
    agent_chinese: Entity<SelectState<crate::controls::Choices>>,
    _subscriptions: Vec<Subscription>,
}

impl FontEditor {
    pub fn focus(&self, window: &mut Window, cx: &mut Context<WorkspaceView>) {
        self.ui_latin
            .update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn new(
        preferences: &crate::config::Preferences,
        window: &mut Window,
        cx: &mut Context<WorkspaceView>,
    ) -> Self {
        let installed = cx.text_system().all_font_names();
        let mut selector = |current: &str| {
            let mut names = installed.clone();
            // Keep an imported font preference when editing unrelated settings.
            if !names.iter().any(|name| name == current) {
                names.push(current.to_owned());
            }
            names.sort_unstable();
            let index = names.iter().position(|name| name == current).unwrap_or(0);
            cx.new(|cx| {
                crate::controls::select_state(
                    names
                        .into_iter()
                        .map(SharedString::from)
                        .collect::<Vec<_>>(),
                    Some(IndexPath::new(index)),
                    window,
                    cx,
                )
                .searchable(true)
            })
        };
        let ui_latin = selector(&preferences.ui_font_family);
        let ui_chinese = selector(&preferences.ui_font_family_zh);
        let terminal_latin = selector(&preferences.font_family);
        let terminal_chinese = selector(&preferences.font_family_zh);
        let agent_latin = selector(&preferences.agent_font_family);
        let agent_chinese = selector(&preferences.agent_font_family_zh);
        let subscriptions = [
            &ui_latin,
            &ui_chinese,
            &terminal_latin,
            &terminal_chinese,
            &agent_latin,
            &agent_chinese,
        ]
        .into_iter()
        .map(|state| cx.observe(state, |_, _, cx| cx.notify()))
        .collect();
        Self {
            ui_latin,
            ui_chinese,
            terminal_latin,
            terminal_chinese,
            agent_latin,
            agent_chinese,
            _subscriptions: subscriptions,
        }
    }

    pub fn apply(
        &self,
        preferences: &mut crate::config::Preferences,
        cx: &Context<WorkspaceView>,
    ) -> Result<()> {
        let value = |state: &Entity<SelectState<crate::controls::Choices>>| {
            state
                .read(cx)
                .selected_value()
                .map(|value| value.to_string())
                .context(crate::i18n::text("请选择字体"))
        };
        preferences.ui_font_family = value(&self.ui_latin)?;
        preferences.ui_font_family_zh = value(&self.ui_chinese)?;
        preferences.font_family = value(&self.terminal_latin)?;
        preferences.font_family_zh = value(&self.terminal_chinese)?;
        preferences.agent_font_family = value(&self.agent_latin)?;
        preferences.agent_font_family_zh = value(&self.agent_chinese)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SettingsPage {
    #[default]
    General,
    Appearance,
    Terminal,
    Providers,
    Proxies,
    Tools,
    Data,
}

struct RecordSave {
    profile: Profile,
    secrets: Vec<(String, zeroize::Zeroizing<String>)>,
    id: Uuid,
}

struct ProviderProbe {
    id: Uuid,
    cancel: tokio_util::sync::CancellationToken,
}

impl Drop for ProviderProbe {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(super) struct RecordEditor {
    pub id: Uuid,
    pub page: SettingsPage,
    form: Form,
    protocol: Entity<SelectState<crate::controls::Choices>>,
    proxy: Entity<SelectState<crate::controls::Choices>>,
    proxy_ids: Vec<Option<Uuid>>,
    models: Vec<Model>,
    model_input: Entity<gpui_kit::component::input::InputState>,
    probe: Option<ProviderProbe>,
    probe_status: Option<(bool, String)>,
    _probe_subscriptions: Vec<Subscription>,
    enabled: bool,
    pub saving: bool,
    pub error: Option<String>,
    _protocol_subscription: Subscription,
}

impl WorkspaceView {
    fn cancel_provider_probe(&mut self) {
        if let Some(editor) = &mut self.record_editor {
            editor.probe = None;
            editor.probe_status = None;
        }
    }

    fn probe_provider(&mut self, apply: bool, cx: &mut Context<Self>) {
        let result = (|| -> Result<_> {
            let editor = self.record_editor.as_ref().context("配置窗口已关闭")?;
            anyhow::ensure!(!editor.saving && editor.probe.is_none(), "请求正在进行");
            let kind = Kind::ALL[editor
                .protocol
                .read(cx)
                .selected_index(cx)
                .map(|index| index.row)
                .unwrap_or(0)];
            let proxy = editor
                .proxy
                .read(cx)
                .selected_index(cx)
                .and_then(|index| editor.proxy_ids.get(index.row))
                .copied()
                .flatten()
                .and_then(|id| {
                    self.store
                        .profile
                        .proxies
                        .iter()
                        .find(|proxy| proxy.id == id)
                })
                .cloned();
            Ok((
                editor.id,
                kind,
                editor.form.value("endpoint", cx),
                zeroize::Zeroizing::new(editor.form.value("key_secret", cx)),
                proxy,
            ))
        })();
        let (id, kind, endpoint, mut key, proxy) = match result {
            Ok(values) => values,
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        // A user-requested connection test is an explicit credential recovery attempt.
        crate::config::retry_secret_access();
        let request = Uuid::new_v4();
        let cancel = tokio_util::sync::CancellationToken::new();
        if let Some(editor) = &mut self.record_editor {
            editor.probe = Some(ProviderProbe {
                id: request,
                cancel: cancel.clone(),
            });
            editor.probe_status = None;
        }
        let events = self.events.clone();
        let stored = self
            .store
            .profile
            .ai_providers
            .iter()
            .find(|provider| provider.id == id)
            .cloned();
        self.launch(async move {
            let result = async {
                if key.is_empty()
                    && let Some(provider) = stored
                {
                    key = tokio::task::spawn_blocking(move || {
                        crate::config::secret(&provider.secret_name())
                    })
                    .await??;
                }
                crate::provider_catalog::fetch(kind, &endpoint, &key, proxy.as_ref(), cancel).await
            }
            .await;
            let _ = events
                .send(BackendEvent::ProviderCatalog {
                    editor: id,
                    request,
                    apply,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    pub(super) fn receive_provider_catalog(
        &mut self,
        id: Uuid,
        request: Uuid,
        apply: bool,
        result: Result<Vec<Model>>,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.record_editor else {
            return;
        };
        if editor.id != id
            || !editor
                .probe
                .as_ref()
                .is_some_and(|probe| probe.id == request)
        {
            return;
        }
        editor.probe = None;
        editor.probe_status = Some(match result {
            Ok(models) => {
                let count = models.len();
                if apply {
                    match crate::provider_catalog::merge(&mut editor.models, models) {
                        Ok(()) => (true, format!("已获取 {count} 个模型")),
                        Err(error) => (false, error.to_string()),
                    }
                } else {
                    (true, format!("连接成功，可读取 {count} 个模型"))
                }
            }
            Err(error) => (false, error.to_string()),
        });
        cx.notify();
    }

    fn add_provider_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.record_editor else {
            return;
        };
        let id = editor.model_input.read(cx).value().trim().to_string();
        if id.is_empty() {
            return;
        }
        if id.len() > 256 || id.contains(['\r', '\n', '\0']) || editor.models.len() >= 512 {
            editor.probe_status = Some((false, crate::i18n::text("模型名称无效或列表已满").into()));
        } else if !editor.models.iter().any(|model| model.id == id) {
            editor.models.push(Model { id });
        }
        editor
            .model_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn render_provider_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self
            .record_editor
            .as_ref()
            .expect("provider editor is open");
        let p = self.palette();
        let query = editor.model_input.read(cx).value().to_lowercase();
        let rows = editor
            .models
            .iter()
            .enumerate()
            .filter(|(_, model)| model.id.to_lowercase().contains(&query))
            .map(|(index, model)| {
                let name = model.id.clone();
                let remove_id = name.clone();
                // Reasoning levels are not configured here: the AI assistant's model
                // menu offers each model's own levels from Rig's catalog.
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .px_2()
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .child(div().flex_1().min_w_0().text_ellipsis().child(name))
                    .child(
                        icon_button(
                            format!("remove-model-{index}"),
                            IconName::X,
                            crate::i18n::text("移除模型"),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(editor) = &mut view.record_editor {
                                editor.models.retain(|model| model.id != remove_id);
                            }
                            cx.notify();
                        })),
                    )
            });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.base))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap_2()
                    .child(crate::i18n::text("模型与可用推理强度"))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                crate::controls::button("fetch-provider-models")
                                    .compact()
                                    .label(crate::i18n::text(if editor.probe.is_some() {
                                        "连接中…"
                                    } else {
                                        "获取模型"
                                    }))
                                    .disabled(editor.probe.is_some() || editor.saving)
                                    .on_click(
                                        cx.listener(|view, _, _, cx| view.probe_provider(true, cx)),
                                    ),
                            )
                            .child(
                                crate::controls::button("test-provider-connection")
                                    .compact()
                                    .label(crate::i18n::text("测试连接"))
                                    .disabled(editor.probe.is_some() || editor.saving)
                                    .on_click(
                                        cx.listener(|view, _, _, cx| {
                                            view.probe_provider(false, cx)
                                        }),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        crate::controls::input(&editor.model_input, cx)
                            .flex_1()
                            .min_w_0(),
                    )
                    .child(
                        crate::controls::button("add-provider-model")
                            .compact()
                            .label(crate::i18n::text("手动添加"))
                            .disabled(editor.saving)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.add_provider_model(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("provider-models-scroll")
                    .max_h(px(190.))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(editor.models.is_empty(), |row| {
                        row.child(
                            div()
                                .py_3()
                                .text_size(px(12.))
                                .text_color(rgb(p.muted))
                                .child(crate::i18n::text("填写密钥后获取模型，也可以手动添加")),
                        )
                    }),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text(
                        "展开模型可多选推理强度；自动始终可用。测试连接仅查询模型列表。",
                    )),
            )
            .children(editor.probe_status.as_ref().map(|(success, message)| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(if *success { p.success } else { p.ansi[1] }))
                    .child(crate::i18n::localize(message.clone()))
            }))
            .into_any_element()
    }
    pub(super) fn render_shell_selector(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let Some(shell) = &form.shell else {
            return div().into_any_element();
        };
        let id = form.id;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(crate::i18n::text("本地 Shell"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap_2()
                    .child(
                        crate::controls::select(&shell.state, cx)
                            .flex_1()
                            .min_w_0()
                            .menu_width(px(520.))
                            .search_placeholder(crate::i18n::text("搜索本地 Shell…"))
                            .accessibility_label(crate::i18n::text("本地 Shell")),
                    )
                    .child(
                        crate::controls::button("browse-local-shell")
                            .ghost()
                            .compact()
                            .icon(gpui_kit::component::Icon::new(IconName::FolderOpen).size_4())
                            .label(crate::i18n::text("浏览…"))
                            .disabled(self.configuration_save.is_some() || self.data_busy.is_some())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.browse_form_path(
                                    id,
                                    "shell",
                                    super::paths::PathKind::File,
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_fonts(&self, form: &Form, cx: &Context<Self>) -> AnyElement {
        let Some(fonts) = &form.fonts else {
            return div().into_any_element();
        };
        let mut preferences = self.store.profile.preferences.clone();
        let _ = fonts.apply(&mut preferences, cx);
        let p = self.palette();
        let field = |state, label| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text(label)),
                )
                .child(
                    crate::controls::select(state, cx)
                        .w_full()
                        .menu_width(px(320.))
                        .search_placeholder(crate::i18n::text("搜索系统字体…"))
                        .accessibility_label(crate::i18n::text(label)),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(
                [
                    (
                        "界面字体",
                        "界面文字、菜单和按钮",
                        &fonts.ui_latin,
                        &fonts.ui_chinese,
                        "界面英文字体",
                        "界面中文字体",
                        "ui_size",
                        crate::fonts::FontPair::ui(&preferences, cx),
                    ),
                    (
                        "终端字体",
                        "终端正文和命令发送器",
                        &fonts.terminal_latin,
                        &fonts.terminal_chinese,
                        "终端英文字体",
                        "终端中文字体",
                        "size",
                        crate::fonts::FontPair::terminal(&preferences, cx),
                    ),
                    (
                        "Agent 字体",
                        "AI 回复、代码和输入框 · 独立于终端",
                        &fonts.agent_latin,
                        &fonts.agent_chinese,
                        "Agent 英文字体",
                        "Agent 中文字体",
                        "agent_size",
                        crate::fonts::FontPair::agent(&preferences, cx),
                    ),
                ]
                .into_iter()
                .map(
                    |(title, detail, latin, chinese, latin_label, chinese_label, size, pair)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded(px(10.))
                            .border_1()
                            .border_color(rgb(p.border))
                            .bg(rgb(p.base))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(crate::i18n::text(title))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(p.muted))
                                            .child(crate::i18n::text(detail)),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(field(latin, latin_label))
                                    .child(field(chinese, chinese_label))
                                    .child(div().w(px(80.)).flex_shrink_0().child(form.field(
                                        size,
                                        crate::i18n::text("字号"),
                                        cx,
                                    ))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .text_size(px(13.))
                                    .text_color(rgb(p.muted))
                                    .child(div().font(pair.latin).child("Aa 0123 / $ git status"))
                                    .child(div().font(pair.chinese).child("中文字体预览")),
                            )
                    },
                ),
            )
            .into_any_element()
    }

    pub(super) fn persist_profile(
        &mut self,
        id: Uuid,
        profile: Profile,
        secrets: Vec<(String, zeroize::Zeroizing<String>)>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.persist_profile_removing_secrets(id, profile, secrets, Vec::new(), cx)
    }

    pub(super) fn persist_profile_removing_secrets(
        &mut self,
        id: Uuid,
        profile: Profile,
        secrets: Vec<(String, zeroize::Zeroizing<String>)>,
        removals: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.configuration_save.is_none() && self.data_busy.is_none(),
            crate::i18n::text("正在保存配置")
        );
        profile.validate()?;
        self.configuration_save = Some(id);
        self.status = Some(crate::i18n::text("正在保存配置").into());
        let directory = self.store.directory.clone();
        let events = self.events.clone();
        self.launch(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<Profile> {
                crate::config::save_with_secret_removals(
                    &crate::config::Store {
                        profile: profile.clone(),
                        directory,
                    },
                    &secrets,
                    &removals,
                )?;
                Ok(profile)
            })
            .await
            .unwrap_or_else(|error| Err(error.into()));
            let _ = events
                .send(BackendEvent::ConfigurationSaved { id, result })
                .await;
        });
        cx.notify();
        Ok(())
    }

    pub(super) fn settings_heading(&self, title: &str, detail: &str) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .mb_5()
            .child(
                div()
                    .text_size(px(21.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(crate::i18n::text(title).to_owned()),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(self.palette().muted))
                    .child(crate::i18n::text(detail).to_owned()),
            )
            .into_any_element()
    }

    pub(super) fn settings_group(
        &self,
        title: &str,
        detail: &str,
        content: AnyElement,
    ) -> AnyElement {
        let p = self.palette();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(crate::i18n::text(title).to_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(crate::i18n::text(detail).to_owned()),
                    ),
            )
            .child(
                div()
                    .p_4()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.base))
                    .child(content),
            )
            .into_any_element()
    }

    fn setting_toggle(
        &self,
        id: &'static str,
        title: &str,
        detail: &str,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette();
        div()
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
                    .flex_1()
                    .min_w_0()
                    .child(crate::i18n::text(title).to_owned())
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(p.muted))
                            .child(crate::i18n::text(detail).to_owned()),
                    ),
            )
            .child(
                Switch::new(id)
                    .accessibility_label(crate::i18n::text(title).to_owned())
                    .color(rgb(p.accent))
                    .cursor_pointer()
                    .checked(checked)
                    .on_click(cx.listener(move |view, value: &bool, _, cx| {
                        if let Some(p) = view
                            .form
                            .as_mut()
                            .and_then(|form| form.preferences.as_mut())
                        {
                            match id {
                                "track-directory" => p.track_directory = *value,
                                "record-history" => p.record_history = *value,
                                "record-sessions" => p.record_sessions = *value,
                                "restore-workspace" => p.restore_workspace = *value,
                                "check-updates" => p.check_updates = *value,
                                "cursor-blink" => p.terminal.cursor_blink = *value,
                                "unfocused-cursor" => p.terminal.unfocused_cursor = *value,
                                "copy-on-select" => p.terminal.copy_on_select = *value,
                                "option-as-alt" => p.terminal.option_as_alt = *value,
                                _ => {}
                            }
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_general_settings(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let p = form
            .preferences
            .as_ref()
            .unwrap_or(&self.store.profile.preferences);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.settings_heading("常规", "语言、启动环境与文件传输"))
            .child(
                self.settings_group(
                    "语言",
                    "设置应用界面的显示语言",
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(crate::i18n::text("界面语言"))
                        .children(
                            form.language
                                .as_ref()
                                .map(|state| crate::controls::select(state, cx).w_full()),
                        )
                        .into_any_element(),
                ),
            )
            .child(
                self.settings_group(
                    "启动与目录",
                    "应用于新建的本地终端会话",
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(self.render_shell_selector(form, cx))
                        .child(self.setting_toggle(
                            "restore-workspace",
                            "恢复工作区",
                            "启动时恢复标签、分屏和本地目录；SSH 会话手动重连",
                            p.restore_workspace,
                            cx,
                        ))
                        .child(form.path_field(
                            "directory",
                            crate::i18n::text("启动目录"),
                            super::paths::PathKind::Directory,
                            cx,
                        ))
                        .child(self.setting_toggle(
                            "track-directory",
                            "目录跟踪",
                            "在会话标题和文件浏览器中显示当前工作目录",
                            p.track_directory,
                            cx,
                        ))
                        .child(self.setting_toggle(
                            "check-updates",
                            "检查新版本",
                            "启动时向 GitHub 查询一次是否有新版本，不发送任何个人信息",
                            p.check_updates,
                            cx,
                        ))
                        .into_any_element(),
                ),
            )
            .child(
                self.settings_group(
                    "文件传输",
                    "每个传输任务独立限速",
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(form.field(
                            "transfer_limit",
                            crate::i18n::text("传输限速（KiB/s，0 为不限速）"),
                            cx,
                        ))
                        .child(form.field(
                            "transfer_concurrency",
                            crate::i18n::text("并发传输数（1–8）"),
                            cx,
                        ))
                        .into_any_element(),
                ),
            )
            .into_any_element()
    }

    fn render_appearance_settings(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let appearance = form
            .appearance
            .unwrap_or(self.store.profile.preferences.appearance);
        let colors = crate::theme::Palette::for_appearance(appearance);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                self.settings_heading("外观与字体", "选择主题，并分别设置界面与终端的中英文字体"),
            )
            .child(
                self.settings_group(
                    "主题",
                    "搜索选择主题，预览实时更新",
                    div()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .children(form.theme.as_ref().map(|theme| {
                            crate::controls::select(&theme.state, cx)
                                .w_full()
                                .menu_width(px(360.))
                                .search_placeholder(crate::i18n::text("搜索主题…"))
                                .accessibility_label(crate::i18n::text("主题"))
                        }))
                        .child(
                            div()
                                .flex()
                                .rounded(px(8.))
                                .border_1()
                                .border_color(rgb(colors.border))
                                .overflow_hidden()
                                .h(px(130.))
                                .child(
                                    div()
                                        .w(px(112.))
                                        .flex_shrink_0()
                                        .p_3()
                                        .bg(rgb(colors.sidebar))
                                        .text_color(rgb(colors.text))
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_3()
                                                .child("Nutty")
                                                .child(
                                                    div()
                                                        .h(px(7.))
                                                        .w(px(72.))
                                                        .rounded_md()
                                                        .bg(rgb(colors.hover)),
                                                )
                                                .child(
                                                    div()
                                                        .h(px(7.))
                                                        .w(px(52.))
                                                        .rounded_md()
                                                        .bg(rgb(colors.hover)),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .justify_center()
                                        .gap_3()
                                        .p_4()
                                        .bg(rgb(colors.terminal))
                                        .child(
                                            div()
                                                .text_color(rgb(colors.text))
                                                .child("~/workspace ❯ git status"),
                                        )
                                        .child(
                                            div()
                                                .text_color(rgb(colors.success))
                                                .child("✓ Working tree clean"),
                                        )
                                        .child(div().flex().gap_2().children(
                                            colors.ansi[1..7].iter().map(|color| {
                                                div().size(px(14.)).rounded(px(3.)).bg(rgb(*color))
                                            }),
                                        )),
                                ),
                        )
                        .into_any_element(),
                ),
            )
            .child(self.render_fonts(form, cx))
            .into_any_element()
    }

    fn render_terminal_settings(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let preferences = form
            .preferences
            .as_ref()
            .unwrap_or(&self.store.profile.preferences);
        let options = &preferences.terminal;
        let row = |fields: &[(&str, &str)]| {
            div()
                .flex()
                .gap_4()
                .children(fields.iter().map(|(key, label)| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(form.field(key, crate::i18n::text(label), cx))
                }))
                .into_any_element()
        };
        let geometry = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(row(&[
                ("line_height", "行高（字体倍数，1–2.5）"),
                ("letter_spacing", "字间距（px，0–8）"),
            ]))
            .child(row(&[
                ("padding_horizontal", "水平内边距（px，0–80）"),
                ("padding_vertical", "垂直内边距（px，0–80）"),
            ]))
            .into_any_element();
        let cursor_fields = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(crate::i18n::text("光标形状"))
                            .children(form.terminal.as_ref().map(|editor| {
                                crate::controls::select(&editor.cursor, cx)
                                    .w_full()
                                    .accessibility_label(crate::i18n::text("光标形状"))
                            })),
                    )
                    .child(div().flex_1().child(form.field(
                        "cursor_width",
                        crate::i18n::text("光标线宽（px，1–5）"),
                        cx,
                    ))),
            )
            .child(form.field(
                "cursor_blink_ms",
                crate::i18n::text("闪烁间隔（ms，200–2000）"),
                cx,
            ));
        let cursor = cursor_fields
            .child(self.setting_toggle(
                "cursor-blink",
                "光标闪烁",
                "终端程序可以临时调整光标样式",
                options.cursor_blink,
                cx,
            ))
            .child(self.setting_toggle(
                "unfocused-cursor",
                "未聚焦时显示空心光标",
                "切换窗格后仍可辨认输入位置",
                options.unfocused_cursor,
                cx,
            ))
            .into_any_element();
        let input = div()
            .flex()
            .flex_col()
            .child(form.field(
                "scroll_speed",
                crate::i18n::text("滚动速度（倍数，0.5–5）"),
                cx,
            ))
            .child(self.setting_toggle(
                "copy-on-select",
                "选中后自动复制",
                "将终端中选中的文字复制到剪贴板",
                options.copy_on_select,
                cx,
            ))
            .child(
                div()
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
                            .flex_1()
                            .min_w_0()
                            .child(crate::i18n::text("终端程序写入剪贴板"))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(self.palette().muted))
                                    .child(crate::i18n::text(
                                        "OSC 52 写入，常见于远端 tmux、vim；读取剪贴板始终询问",
                                    )),
                            ),
                    )
                    .children(form.terminal.as_ref().map(|editor| {
                        crate::controls::select(&editor.clipboard, cx)
                            .w(px(160.))
                            .flex_shrink_0()
                            .accessibility_label(crate::i18n::text("终端程序写入剪贴板"))
                    })),
            )
            .when(cfg!(target_os = "macos"), |this| {
                this.child(self.setting_toggle(
                    "option-as-alt",
                    "Option 作为 Alt 键",
                    "关闭后使用系统的 Option 字符输入",
                    options.option_as_alt,
                    cx,
                ))
            })
            .into_any_element();
        let history = div()
            .flex()
            .flex_col()
            .child(form.field("scrollback", crate::i18n::text("滚动历史行数"), cx))
            .child(self.setting_toggle(
                "record-history",
                "保存命令历史",
                "将 Shell 命令记录到本地文件",
                preferences.record_history,
                cx,
            ))
            .child(self.setting_toggle(
                "record-sessions",
                "保存终端会话日志",
                "将终端输出记录到本地文件",
                preferences.record_sessions,
                cx,
            ))
            .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.settings_heading("终端", "排版、光标与输入行为；保存后应用于所有窗格"))
            .child(self.settings_group(
                "排版与留白",
                "字符在整行中垂直居中，中文保持双列对齐",
                geometry,
            ))
            .child(self.settings_group("光标", "设置终端的默认光标样式", cursor))
            .child(self.settings_group("输入与滚动", "调整选区、键盘和滚轮行为", input))
            .child(self.settings_group(
                "历史与记录",
                "会话内历史始终可用，文件记录按需开启",
                history,
            ))
            .into_any_element()
    }

    pub(super) fn render_settings_page(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        let page = self.settings_page;
        let content = match page {
            SettingsPage::General => self.render_general_settings(form, cx),
            SettingsPage::Appearance => self.render_appearance_settings(form, cx),
            SettingsPage::Terminal => self.render_terminal_settings(form, cx),
            SettingsPage::Providers => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_4()
                        .child(self.settings_heading("AI 提供方", "每个终端独立选择提供方、模型与推理强度。密钥保存在系统钥匙串。"))
                        .child(
                            crate::controls::button("add-provider")
                                .primary()
                                .compact()
                                .label(crate::i18n::text("添加提供方"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.edit_record(SettingsPage::Providers, None, window, cx)
                                })),
                        ),
                )

                .children(self.store.profile.ai_providers.iter().map(|provider| {
                    let id = provider.id;
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .p_4()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(rgb(p.border))
                        .bg(rgb(p.base))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_ellipsis().child(provider.name.clone()))
                                .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                                    crate::i18n::localize(format!(
                                        "{} · {} 个模型{}",
                                        provider.kind.label(),
                                        provider.models.len(),
                                        if provider.enabled {
                                            ""
                                        } else {
                                            crate::i18n::text(" · 已禁用")
                                        }
                                    )),
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    icon_button(
                                        format!("edit-provider-{id}"),
                                        IconName::Settings,
                                        crate::i18n::text("编辑"),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.edit_record(
                                                SettingsPage::Providers,
                                                Some(id),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                                .child(
                                    icon_button(
                                        format!("remove-provider-{id}"),
                                        IconName::X,
                                        crate::i18n::text("移除提供方"),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.remove_record(
                                                SettingsPage::Providers,
                                                id,
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                ),
                        )
                }))
                .into_any_element(),
            SettingsPage::Proxies => div()
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_4()
                        .child(self.settings_heading("网络代理", "SSH 支持 HTTP CONNECT 与 SOCKS5；AI 提供方可选择 HTTP、HTTPS 或 SOCKS5 代理。"))
                        .child(
                            crate::controls::button("add-proxy")
                                .primary()
                                .compact()
                                .label(crate::i18n::text("添加代理"))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.edit_record(SettingsPage::Proxies, None, window, cx)
                                })),
                        ),
                )

                .children(self.store.profile.proxies.iter().map(|proxy| {
                    let id = proxy.id;
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .p_4()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(rgb(p.border))
                        .bg(rgb(p.base))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_ellipsis().child(proxy.name.clone()))
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(p.muted))
                                        .child(proxy.url.clone()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .gap_1()
                                .child(
                                    icon_button(
                                        format!("edit-proxy-{id}"),
                                        IconName::Settings,
                                        crate::i18n::text("编辑"),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.edit_record(
                                                SettingsPage::Proxies,
                                                Some(id),
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                                .child(
                                    icon_button(
                                        format!("remove-proxy-{id}"),
                                        IconName::X,
                                        crate::i18n::text("移除代理"),
                                    )
                                    .on_click(cx.listener(
                                        move |view, _, window, cx| {
                                            view.remove_record(
                                                SettingsPage::Proxies,
                                                id,
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                ),
                        )
                }))
                .into_any_element(),
            SettingsPage::Tools => self.render_integrations_settings(cx),
            SettingsPage::Data => self.render_data_settings(cx),
        };
        div()
            .flex()
            .h_full()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w(px(204.))
                    .flex_shrink_0()
                    .px_3()
                    .py_4()
                    .border_r_1()
                    .border_color(rgb(p.border))
                    .children(
                        [
                            (
                                SettingsPage::General,
                                crate::i18n::text("常规"),
                                IconName::Settings,
                            ),
                            (
                                SettingsPage::Appearance,
                                crate::i18n::text("外观与字体"),
                                IconName::Palette,
                            ),
                            (
                                SettingsPage::Terminal,
                                crate::i18n::text("终端"),
                                IconName::SquareTerminal,
                            ),
                            (
                                SettingsPage::Providers,
                                crate::i18n::text("AI 提供方"),
                                IconName::Sparkles,
                            ),
                            (
                                SettingsPage::Proxies,
                                crate::i18n::text("网络代理"),
                                IconName::Network,
                            ),
                            (
                                SettingsPage::Tools,
                                crate::i18n::text("工具与扩展"),
                                IconName::Terminal,
                            ),
                            (
                                SettingsPage::Data,
                                crate::i18n::text("导入与备份"),
                                IconName::FolderOpen,
                            ),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (target, label, icon))| {
                            let button = crate::controls::button(format!("settings-page-{index}"))
                                .h(px(42.))
                                .justify_start()
                                .accessibility_label(label)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_start()
                                        .w_full()
                                        .gap_3()
                                        .child(gpui_kit::component::Icon::new(icon).size_5())
                                        .child(label),
                                );
                            let button = button.ghost().when(page == target, |button| {
                                button.bg(rgb(p.hover)).text_color(rgb(p.accent))
                            });
                            button.on_click(cx.listener(move |view, _, window, cx| {
                                view.settings_page = target;
                                if target == SettingsPage::Tools && !view.agents_scanned {
                                    view.scan_local_agents(cx);
                                }
                                if let Some(form) = &view.form {
                                    match target {
                                        SettingsPage::General => {
                                            if let Some(language) = &form.language {
                                                language.update(cx, |state, cx| {
                                                    state.focus(window, cx)
                                                });
                                            }
                                        }
                                        SettingsPage::Appearance => {
                                            if let Some(fonts) = &form.fonts {
                                                fonts.focus(window, cx);
                                            }
                                        }
                                        SettingsPage::Terminal => {
                                            if let Some(input) = form.fields.get("line_height") {
                                                input.update(cx, |input, cx| {
                                                    input.focus(window, cx)
                                                });
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                cx.notify();
                            }))
                        }),
                    ),
            )
            .child(
                div()
                    .id("settings-page-scroll")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px_7()
                    .py_6()
                    .bg(rgb(p.sidebar))
                    .child(content),
            )
            .into_any_element()
    }

    pub(super) fn edit_record(
        &mut self,
        page: SettingsPage,
        id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        let provider = id.and_then(|id| {
            self.store
                .profile
                .ai_providers
                .iter()
                .find(|item| item.id == id)
        });
        let proxy_record =
            id.and_then(|id| self.store.profile.proxies.iter().find(|item| item.id == id));
        let id = id.unwrap_or_else(Uuid::new_v4);
        let kind = provider.map(|item| item.kind).unwrap_or_default();
        let proxy_url = proxy_record.and_then(|proxy| url::Url::parse(&proxy.url).ok());
        let proxy_host = proxy_url
            .as_ref()
            .and_then(url::Url::host_str)
            .unwrap_or("127.0.0.1");
        let proxy_port = proxy_url
            .as_ref()
            .and_then(url::Url::port_or_known_default)
            .unwrap_or(1080)
            .to_string();
        let proxy_protocol = proxy_url
            .as_ref()
            .and_then(|url| {
                crate::network::Protocol::ALL
                    .iter()
                    .position(|protocol| protocol.scheme() == url.scheme())
            })
            .unwrap_or(2);
        let fields = match page {
            SettingsPage::Providers => vec![
                (
                    "name",
                    provider
                        .map(|item| item.name.as_str())
                        .unwrap_or(kind.label()),
                ),
                (
                    "endpoint",
                    provider
                        .map(|item| item.endpoint.as_str())
                        .unwrap_or(kind.endpoint()),
                ),
                ("key_secret", ""),
            ],
            _ => vec![
                (
                    "name",
                    proxy_record.map(|item| item.name.as_str()).unwrap_or(""),
                ),
                ("proxy_host", proxy_host),
                ("proxy_port", &proxy_port),
                (
                    "username",
                    proxy_record
                        .map(|item| item.username.as_str())
                        .unwrap_or(""),
                ),
                ("key_secret", ""),
            ],
        };
        let form = Form::new(&fields, window, cx);
        let protocols = if page == SettingsPage::Providers {
            Kind::ALL
                .into_iter()
                .map(|kind| SharedString::from(kind.label()))
                .collect::<Vec<_>>()
        } else {
            crate::network::Protocol::ALL
                .into_iter()
                .map(|protocol| SharedString::from(crate::i18n::text(protocol.label())))
                .collect()
        };
        let protocol_index = if page == SettingsPage::Providers {
            kind as usize
        } else {
            proxy_protocol
        };
        let protocol = cx.new(|cx| {
            crate::controls::select_state(
                protocols,
                Some(IndexPath::new(protocol_index)),
                window,
                cx,
            )
        });
        let mut proxy_ids = vec![None];
        let mut proxy_names = vec![SharedString::from(crate::i18n::text("直连"))];
        for proxy in &self.store.profile.proxies {
            proxy_ids.push(Some(proxy.id));
            proxy_names.push(proxy.name.clone().into());
        }
        let selected = provider
            .and_then(|provider| provider.proxy)
            .and_then(|id| proxy_ids.iter().position(|proxy| *proxy == Some(id)))
            .unwrap_or(0);
        let proxy = cx.new(|cx| {
            crate::controls::select_state(proxy_names, Some(IndexPath::new(selected)), window, cx)
        });
        let models = provider
            .map(|provider| provider.models.clone())
            .unwrap_or_default();
        let model_input = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx)
                .placeholder(crate::i18n::text("搜索或输入模型 ID…"))
        });
        let mut probe_subscriptions = Vec::new();
        probe_subscriptions.push(cx.subscribe_in(
            &model_input,
            window,
            |view, _, event: &gpui_kit::component::input::InputEvent, window, cx| {
                if matches!(
                    event,
                    gpui_kit::component::input::InputEvent::PressEnter { .. }
                ) {
                    view.add_provider_model(window, cx);
                } else {
                    cx.notify();
                }
            },
        ));
        for field in ["endpoint", "key_secret"] {
            if let Some(input) = form.fields.get(field) {
                probe_subscriptions.push(cx.subscribe(
                    input,
                    |view, _, event: &gpui_kit::component::input::InputEvent, cx| {
                        if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                            view.cancel_provider_probe();
                            cx.notify();
                        }
                    },
                ));
            }
        }
        probe_subscriptions.push(cx.subscribe(
            &proxy,
            |view, _, _: &SelectEvent<crate::controls::Choices>, cx| {
                view.cancel_provider_probe();
                cx.notify();
            },
        ));
        let protocol_subscription = cx.subscribe_in(
            &protocol,
            window,
            |view, state, _: &SelectEvent<crate::controls::Choices>, window, cx| {
                view.cancel_provider_probe();
                let Some(editor) = &view.record_editor else {
                    return;
                };
                if editor.page != SettingsPage::Providers {
                    return;
                }
                let endpoint = editor.form.value("endpoint", cx);
                let selected = state
                    .read(cx)
                    .selected_index(cx)
                    .map(|index| index.row)
                    .unwrap_or(0);
                let kind = Kind::ALL[selected];
                if Kind::ALL.iter().any(|kind| endpoint == kind.endpoint())
                    && let Some(input) = editor.form.fields.get("endpoint")
                {
                    input.update(cx, |input, cx| input.set_value(kind.endpoint(), window, cx));
                }
                let name = editor.form.value("name", cx);
                if (name.is_empty() || Kind::ALL.iter().any(|kind| name == kind.label()))
                    && let Some(input) = editor.form.fields.get("name")
                {
                    input.update(cx, |input, cx| input.set_value(kind.label(), window, cx));
                }
                cx.notify();
            },
        );
        form.fields["name"].update(cx, |input, cx| input.focus(window, cx));
        self.record_editor = Some(RecordEditor {
            id,
            page,
            form,
            protocol,
            proxy,
            proxy_ids,
            models,
            model_input,
            probe: None,
            probe_status: None,
            _probe_subscriptions: probe_subscriptions,
            enabled: provider.is_none_or(|provider| provider.enabled),
            saving: false,
            error: None,
            _protocol_subscription: protocol_subscription,
        });
        cx.notify();
    }

    pub(super) fn render_record_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.record_editor.as_ref().expect("record editor is open");
        let provider = editor.page == SettingsPage::Providers;
        let p = self.palette();
        let header = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .px_6()
            .py_4()
            .border_b_1()
            .border_color(rgb(p.border))
            .child(div().text_size(px(18.)).child(if provider {
                crate::i18n::text("配置 AI 提供方")
            } else {
                crate::i18n::text("配置网络代理")
            }))
            .child(
                icon_button(
                    "close-record",
                    IconName::X,
                    crate::i18n::text(if self.connection_proxy_editor.is_some() {
                        "返回连接设置"
                    } else {
                        "返回设置"
                    }),
                )
                .disabled(editor.saving)
                .on_click(cx.listener(|view, _, window, cx| {
                    view.record_editor = None;
                    view.connection_proxy_editor = None;
                    if let Some(form) = &view.form
                        && view.modal == Some(super::Modal::Connection)
                    {
                        form.fields["name"].update(cx, |input, cx| input.focus(window, cx));
                    } else {
                        view.modal_focus.focus(window, cx);
                    }
                    cx.notify();
                })),
            );
        let mut content = div()
            .id("record-editor-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .gap_4()
            .p_6()
            .min_h_0()
            .overflow_y_scroll()
            .child(editor.form.field("name", crate::i18n::text("名称"), cx));
        content = if provider {
            content
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(crate::i18n::text("提供商"))
                            .child(crate::controls::select(&editor.protocol, cx).w_full()),
                    )
                    .child(
                        editor
                            .form
                            .field("endpoint", crate::i18n::text("API 地址"), cx),
                    )
                    .children(
                        crate::network::cleartext_remote(&editor.form.value("endpoint", cx))
                            .then(|| {
                                super::forwarding::warning_notice(
                                    "此 API 地址使用 http 且不在本机：对话内容和 API 密钥都会明文传输，可能被网络中的设备截获。请尽量改用 https。",
                                )
                            }),
                    )
                    .child(editor.form.field(
                        "key_secret",
                        crate::i18n::text("API 密钥 · 留空保留，Ollama 可留空"),
                        cx,
                    ))
                    .child(self.render_provider_models(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(crate::i18n::text("网络代理"))
                            .child(crate::controls::select(&editor.proxy, cx).w_full()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(crate::i18n::text("启用此提供方"))
                            .child(
                                Switch::new("provider-enabled")
                                    .cursor_pointer()
                                    .accessibility_label(crate::i18n::text("启用此提供方"))
                                    .checked(editor.enabled)
                                    .on_click(cx.listener(|view, value: &bool, _, cx| {
                                        if let Some(editor) = &mut view.record_editor {
                                            editor.enabled = *value;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    )
        } else {
            content
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(crate::i18n::text("代理类型"))
                        .child(crate::controls::select(&editor.protocol, cx).w_full()),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_1().min_w_0().child(editor.form.field(
                            "proxy_host",
                            crate::i18n::text("代理主机"),
                            cx,
                        )))
                        .child(div().w(px(120.)).child(editor.form.field(
                            "proxy_port",
                            crate::i18n::text("端口"),
                            cx,
                        ))),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text(
                            "SSH 支持 HTTP CONNECT / SOCKS5；HTTPS 代理用于 AI 与 MCP",
                        )),
                )
                .child(
                    editor
                        .form
                        .field("username", crate::i18n::text("用户名 · 可选"), cx),
                )
                .child(editor.form.field(
                    "key_secret",
                    crate::i18n::text("代理密码 · 留空保留"),
                    cx,
                ))
        };
        let footer = div()
            .flex()
            .flex_shrink_0()
            .justify_end()
            .px_6()
            .py_4()
            .border_t_1()
            .border_color(rgb(p.border))
            .gap_2()
            .children(editor.error.as_ref().map(|error| {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(rgb(0xf17c82))
                    .child(crate::i18n::localize(error.clone()))
            }))
            .children(
                (editor.page == SettingsPage::Providers
                    && self
                        .store
                        .profile
                        .ai_providers
                        .iter()
                        .any(|provider| provider.id == editor.id))
                .then(|| {
                    crate::controls::button("use-provider-key-once")
                        .ghost()
                        .compact()
                        .label(crate::i18n::text("本次使用密钥"))
                        .tooltip(crate::i18n::text(
                            "仅使用已保存的提供方设置；密钥不写入磁盘，退出时清除",
                        ))
                        .disabled(
                            editor.saving
                                || editor.probe.is_some()
                                || editor.form.value("key_secret", cx).trim().is_empty(),
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.use_provider_key_once(cx)))
                }),
            )
            .child(
                crate::controls::button("save-record")
                    .primary()
                    .label(if editor.saving {
                        crate::i18n::text("保存中…")
                    } else {
                        crate::i18n::text("保存")
                    })
                    .disabled(editor.saving || editor.probe.is_some())
                    .on_click(cx.listener(|view, _, _, cx| view.save_record(cx))),
            );
        modal_container(p)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(640.))
                    .max_w(gpui_kit::relative(0.95))
                    .h(gpui_kit::relative(0.9))
                    .max_h(px(720.))
                    .min_h_0()
                    .overflow_hidden()
                    .rounded_lg()
                    .bg(rgb(p.sidebar))
                    .border_1()
                    .border_color(rgb(p.border))
                    .child(header)
                    .child(content)
                    .child(footer),
            )
            .into_any_element()
    }

    fn use_provider_key_once(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            let editor = self.record_editor.as_ref().context("配置已关闭")?;
            anyhow::ensure!(
                editor.page == SettingsPage::Providers && !editor.saving && editor.probe.is_none(),
                "正在保存配置"
            );
            let provider = self
                .store
                .profile
                .ai_providers
                .iter()
                .find(|provider| provider.id == editor.id)
                .context("请先保存提供方设置")?;
            let protocol = editor
                .protocol
                .read(cx)
                .selected_index(cx)
                .and_then(|index| Kind::ALL.get(index.row))
                .copied();
            anyhow::ensure!(
                protocol == Some(provider.kind)
                    && editor.form.value("endpoint", cx).trim() == provider.endpoint.trim(),
                "请先保存更改的提供方地址和协议"
            );
            let key = zeroize::Zeroizing::new(editor.form.value("key_secret", cx));
            crate::config::use_session_secret(&provider.secret_name(), &key)
        })();
        match result {
            Ok(()) => {
                self.record_editor = None;
                self.status = Some(
                    crate::i18n::text("密钥已用于本次运行，退出时清除；请返回 AI 助手重试").into(),
                );
            }
            Err(error) => self.status = Some(crate::i18n::localize(error.to_string())),
        }
        cx.notify();
    }

    fn save_record(&mut self, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        if let Some(editor) = &mut self.record_editor {
            editor.error = None;
        }
        let result = (|| -> Result<RecordSave> {
            let editor = self
                .record_editor
                .as_ref()
                .context(crate::i18n::text("配置窗口已关闭"))?;
            let mut profile = self.store.profile.clone();
            let mut secrets = Vec::new();
            let password = zeroize::Zeroizing::new(editor.form.value("key_secret", cx));
            if editor.page == SettingsPage::Providers {
                let kind = Kind::ALL[editor
                    .protocol
                    .read(cx)
                    .selected_index(cx)
                    .map(|index| index.row)
                    .unwrap_or(0)];
                let models = editor.models.clone();
                let provider = Provider {
                    id: editor.id,
                    name: editor.form.value("name", cx),
                    kind,
                    endpoint: editor.form.value("endpoint", cx),
                    models,
                    enabled: editor.enabled,
                    proxy: editor
                        .proxy
                        .read(cx)
                        .selected_index(cx)
                        .and_then(|index| editor.proxy_ids.get(index.row))
                        .copied()
                        .flatten(),
                };
                provider.validate()?;
                if !password.is_empty() {
                    secrets.push((provider.secret_name(), password));
                }
                if let Some(previous) = profile
                    .ai_providers
                    .iter_mut()
                    .find(|item| item.id == provider.id)
                {
                    *previous = provider;
                } else {
                    profile.ai_providers.push(provider);
                }
            } else {
                let proxy = Proxy {
                    id: editor.id,
                    name: editor.form.value("name", cx),
                    url: crate::network::Protocol::ALL[editor
                        .protocol
                        .read(cx)
                        .selected_index(cx)
                        .map(|index| index.row)
                        .unwrap_or(2)]
                    .url(
                        &editor.form.value("proxy_host", cx),
                        editor
                            .form
                            .value("proxy_port", cx)
                            .parse()
                            .context(crate::i18n::text("代理端口无效"))?,
                    )?,
                    username: editor.form.value("username", cx),
                };
                proxy.validate()?;
                if self.connection_proxy_editor.is_some() {
                    anyhow::ensure!(
                        proxy.supports_ssh(),
                        "SSH 的 HTTPS 代理需配置 SOCKS5 或 HTTP CONNECT 端点"
                    );
                }
                if !password.is_empty() {
                    secrets.push((format!("proxy-{}", proxy.id), password));
                }
                if let Some(previous) = profile.proxies.iter_mut().find(|item| item.id == proxy.id)
                {
                    *previous = proxy;
                } else {
                    profile.proxies.push(proxy);
                }
            }
            profile.validate()?;
            Ok(RecordSave {
                profile,
                secrets,
                id: editor.id,
            })
        })();
        match result {
            Ok(RecordSave {
                profile,
                secrets,
                id,
            }) => {
                if let Some(editor) = &mut self.record_editor {
                    editor.saving = true;
                }
                if let Err(error) = self.persist_profile(id, profile, secrets, cx) {
                    if let Some(editor) = &mut self.record_editor {
                        editor.saving = false;
                        editor.error = Some(error.to_string());
                    }
                    self.status = Some(error.to_string());
                }
            }
            Err(error) => {
                if let Some(editor) = &mut self.record_editor {
                    editor.error = Some(format!("{error:#}"));
                }
                self.status = Some(format!("{error:#}"));
            }
        }
        cx.notify();
    }

    fn remove_record(
        &mut self,
        page: SettingsPage,
        id: Uuid,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        let mut profile = self.store.profile.clone();
        match page {
            SettingsPage::Providers => profile.ai_providers.retain(|provider| provider.id != id),
            _ => profile.proxies.retain(|proxy| proxy.id != id),
        }
        if let Err(error) = self.persist_profile(id, profile, Vec::new(), cx) {
            self.status = Some(error.to_string());
        }
        cx.notify();
    }

    pub(super) fn refresh_provider_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let providers: Vec<_> = self
            .store
            .profile
            .ai_providers
            .iter()
            .filter(|provider| provider.enabled)
            .collect();
        let mut names: Vec<SharedString> = providers
            .iter()
            .map(|provider| provider.name.clone().into())
            .collect();
        names.extend(
            self.store
                .profile
                .agents
                .iter()
                .filter(|agent| agent.enabled)
                .map(|agent| format!("ACP: {}", agent.name).into()),
        );
        for pane in self.panes.values_mut() {
            let name = pane.assistant.provider.read(cx).selected_value();
            let selected = name
                .and_then(|name| names.iter().position(|item| item == name))
                .unwrap_or(0);
            let mut models = if selected >= providers.len() {
                vec![SharedString::from("Agent default")]
            } else {
                providers
                    .get(selected)
                    .map(|provider| {
                        provider
                            .models
                            .iter()
                            .map(|model| model.id.clone().into())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let model = pane.assistant.model.read(cx).selected_value();
            let selected_model = model
                .and_then(|model| models.iter().position(|item| item == model))
                .unwrap_or(0);
            pane.assistant.provider.update(cx, |state, cx| {
                state.set_items(names.clone().into(), window, cx);
                state.set_selected_index(Some(IndexPath::new(selected)), window, cx);
            });
            pane.assistant.model.update(cx, |state, cx| {
                state.set_items(std::mem::take(&mut models).into(), window, cx);
                state.set_selected_index(Some(IndexPath::new(selected_model)), window, cx);
            });
        }
        for id in self.panes.keys().copied().collect::<Vec<_>>() {
            self.refresh_reasoning(id, window, cx);
        }
    }
}
