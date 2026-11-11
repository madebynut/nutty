//! Pane-local command composer, Shell/Agent surface and AI command dispatch.

use anyhow::Result;
use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::{
    Icon,
    button::ButtonVariants as _,
    input::{InputEvent, TextareaState},
    menu::{DropdownMenu, PopupMenuItem},
    popover::Popover,
    select::{SelectEvent, SelectState},
    text::{TextView, TextViewStyle},
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, SharedString, Subscription, Window,
    div, prelude::*, px, rgb,
};
use std::cell::Cell;
use uuid::Uuid;

use super::{
    BackendEvent, WorkspaceView, icon_button,
    panels::{self, DOCK_BAR, PanelEdge},
};
use crate::{
    ai,
    assistant_session::{CommandState, Mode, Session},
    command,
    provider::{Provider, Reasoning, Selection},
};

pub(super) struct PaneAssistant {
    pub agent_settings: super::agent_settings::AgentSettings,
    pub permissions: Vec<crate::acp::Permission>,
    pub command: Entity<TextareaState>,
    pub command_error: Option<String>,
    pub prompt: Entity<TextareaState>,
    pub prompt_expanded: bool,
    pub prompt_max_rows: Cell<usize>,
    pub session: Session,
    pub provider: Entity<SelectState<crate::controls::Choices>>,
    pub model: Entity<SelectState<crate::controls::Choices>>,
    pub reasoning: Entity<SelectState<crate::controls::Choices>>,
    pub picker: Entity<super::model_picker::ModelPicker>,
    _inputs: Vec<Subscription>,
}

impl PaneAssistant {
    pub fn new(
        id: Uuid,
        providers: &[Provider],
        agents: &[crate::integrations::Agent],
        window: &mut Window,
        cx: &mut Context<WorkspaceView>,
    ) -> Self {
        let workspace = cx.entity().downgrade();
        let picker = cx.new(|cx| super::model_picker::ModelPicker::new(workspace, id, window, cx));
        let command = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(crate::i18n::text("多行命令、管道、脚本… Enter 换行"))
        });
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .scroll_beyond_last_line(Some(0))
                .scroll_bounce(false)
                .placeholder(crate::i18n::text("询问命令，分析此终端的问题…"))
        });
        let mut inputs: Vec<_> = [&command, &prompt]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, move |workspace, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Focus) {
                        workspace.activate_pane(id, cx);
                    }
                    cx.notify();
                })
            })
            .collect();
        // Wrapping can change after layout or resizing, without an InputEvent.
        inputs.push(cx.observe(&prompt, |_, _, cx| cx.notify()));
        let enabled: Vec<_> = providers
            .iter()
            .filter(|provider| provider.enabled)
            .collect();
        let provider = cx.new(|cx| {
            crate::controls::select_state(
                enabled
                    .iter()
                    .map(|provider| SharedString::from(provider.name.clone()))
                    .chain(
                        agents
                            .iter()
                            .filter(|agent| agent.enabled)
                            .map(|agent| SharedString::from(format!("ACP: {}", agent.name))),
                    )
                    .collect::<Vec<_>>(),
                Some(gpui_kit::component::IndexPath::new(0)),
                window,
                cx,
            )
            .searchable(true)
        });
        let first = enabled.first().copied();
        let model = cx.new(|cx| {
            crate::controls::select_state(
                first
                    .map(|provider| {
                        provider
                            .models
                            .iter()
                            .map(|model| SharedString::from(model.id.clone()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                Some(gpui_kit::component::IndexPath::new(0)),
                window,
                cx,
            )
            .searchable(true)
        });
        let reasoning = cx.new(|cx| {
            crate::controls::select_state(
                vec![SharedString::from(Reasoning::Auto.display_label())],
                Some(gpui_kit::component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        inputs.push(cx.subscribe_in(
            &provider,
            window,
            move |workspace, _, event: &SelectEvent<crate::controls::Choices>, window, cx| {
                let SelectEvent::Confirm(Some(name)) = event else {
                    return;
                };
                workspace.activate_pane(id, cx);
                if name.starts_with("ACP: ") {
                    if let Some(pane) = workspace.panes.get_mut(&id) {
                        pane.assistant.session.reset();
                        pane.assistant.permissions.clear();
                        pane.assistant.model.update(cx, |state, cx| {
                            state.set_items(
                                vec![SharedString::from("Agent default")].into(),
                                window,
                                cx,
                            );
                            state.set_selected_index(
                                Some(gpui_kit::component::IndexPath::new(0)),
                                window,
                                cx,
                            );
                        });
                    }
                    workspace.refresh_reasoning(id, window, cx);
                    workspace.load_agent_configuration(id, false, cx);
                    cx.notify();
                    return;
                }
                let Some(provider) = workspace
                    .store
                    .profile
                    .ai_providers
                    .iter()
                    .find(|provider| provider.enabled && provider.name == name.as_ref())
                    .cloned()
                else {
                    return;
                };
                workspace.activate_pane(id, cx);
                if let Some(pane) = workspace.panes.get_mut(&id) {
                    pane.assistant.session.reset();
                    pane.assistant.model.update(cx, |model, cx| {
                        model.set_items(
                            provider
                                .models
                                .iter()
                                .map(|model| SharedString::from(model.id.clone()))
                                .collect::<Vec<_>>()
                                .into(),
                            window,
                            cx,
                        );
                        model.set_selected_index(
                            Some(gpui_kit::component::IndexPath::new(0)),
                            window,
                            cx,
                        );
                    });
                }
                workspace.refresh_reasoning(id, window, cx);
                cx.notify();
            },
        ));
        inputs.push(cx.subscribe_in(
            &model,
            window,
            move |workspace, _, event: &SelectEvent<crate::controls::Choices>, window, cx| {
                if !matches!(event, SelectEvent::Confirm(Some(_))) {
                    return;
                }
                workspace.activate_pane(id, cx);
                if let Some(pane) = workspace.panes.get_mut(&id) {
                    pane.assistant.session.stop();
                }
                workspace.refresh_reasoning(id, window, cx);
                cx.notify();
            },
        ));
        inputs.push(cx.subscribe(
            &reasoning,
            move |workspace, _, event: &SelectEvent<crate::controls::Choices>, cx| {
                if matches!(event, SelectEvent::Confirm(Some(_))) {
                    workspace.activate_pane(id, cx);
                    cx.notify();
                }
            },
        ));
        Self {
            agent_settings: super::agent_settings::AgentSettings::default(),
            permissions: Vec::new(),
            command,
            command_error: None,
            prompt,
            prompt_expanded: false,
            prompt_max_rows: Cell::new(6),
            session: Session::new(),
            provider,
            model,
            reasoning,
            picker,
            _inputs: inputs,
        }
    }
}

impl WorkspaceView {
    pub(super) fn pane_selection(&self, id: Uuid, cx: &gpui_kit::App) -> Result<Selection> {
        let pane = self
            .panes
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!(crate::i18n::text("终端已关闭")))?;
        let name = pane
            .assistant
            .provider
            .read(cx)
            .selected_value()
            .ok_or_else(|| anyhow::anyhow!(crate::i18n::text("请配置 AI 提供方")))?;
        let provider = self
            .store
            .profile
            .ai_providers
            .iter()
            .find(|provider| provider.enabled && provider.name == name.as_ref())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(crate::i18n::text("提供方不存在")))?;
        let model = pane
            .assistant
            .model
            .read(cx)
            .selected_value()
            .map(|value| value.to_string())
            .ok_or_else(|| anyhow::anyhow!(crate::i18n::text("请选择模型")))?;
        let reasoning = pane
            .assistant
            .reasoning
            .read(cx)
            .selected_value()
            .and_then(|name| Reasoning::from_display_label(name.as_ref()))
            .unwrap_or_default();
        let proxy = provider.proxy.and_then(|id| {
            self.store
                .profile
                .proxies
                .iter()
                .find(|proxy| proxy.id == id)
                .cloned()
        });
        let selection = Selection {
            provider,
            model,
            reasoning,
            proxy,
        };
        selection.validate()?;
        Ok(selection)
    }

    pub(super) fn refresh_reasoning(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let levels = self
            .panes
            .get(&id)
            .and_then(|pane| {
                let provider = pane.assistant.provider.read(cx).selected_value()?;
                let model = pane.assistant.model.read(cx).selected_value()?;
                self.store
                    .profile
                    .ai_providers
                    .iter()
                    .find(|candidate| candidate.enabled && candidate.name == provider.as_ref())?
                    .models
                    .iter()
                    .find(|candidate| candidate.id == model.as_ref())
                    .map(|model| model.reasoning.clone())
            })
            .unwrap_or_default();
        let levels = Reasoning::available(&levels);
        let items: Vec<SharedString> = levels
            .iter()
            .map(|level| SharedString::from(level.display_label()))
            .collect();
        if let Some(pane) = self.panes.get_mut(&id) {
            let previous = pane
                .assistant
                .reasoning
                .read(cx)
                .selected_value()
                .and_then(|name| Reasoning::from_display_label(name.as_ref()))
                .unwrap_or_default();
            let selected = levels
                .iter()
                .position(|level| *level == previous)
                .unwrap_or(0);
            pane.assistant.reasoning.update(cx, |reasoning, cx| {
                reasoning.set_items(items.into(), window, cx);
                reasoning.set_selected_index(
                    Some(gpui_kit::component::IndexPath::new(selected)),
                    window,
                    cx,
                );
            });
        }
    }
    pub(super) fn activate_pane(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.panes.contains_key(&id) {
            if let Some(tab) = self
                .tabs
                .iter_mut()
                .find(|tab| tab.layout.panes().contains(&id))
            {
                tab.active_pane = id;
                self.active_tab = Some(tab.id);
            }
            if self.active_pane != Some(id) {
                self.active_pane = Some(id);
                self.reset_files(cx);
            }
        }
    }

    pub(super) fn set_pane_mode(
        &mut self,
        id: Uuid,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_pane(id, cx);
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.dock.set_mode(mode);
            if mode == Mode::Shell || !pane.dock_expanded() {
                pane.view
                    .update(cx, |terminal, cx| terminal.focus(window, cx));
            } else {
                pane.assistant
                    .prompt
                    .update(cx, |prompt, cx| prompt.focus(window, cx));
            }
        }
        cx.notify();
    }

    pub(super) fn render_mode_switch(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let mode = self.panes.get(&id).map(|pane| pane.dock.mode());
        div()
            .flex()
            .items_center()
            .gap_1()
            .ml_2()
            .children(
                [
                    (Mode::Shell, "Shell", IconName::Terminal),
                    (Mode::Agent, "Agent", IconName::Sparkles),
                ]
                .into_iter()
                .map(|(value, label, icon)| {
                    let button = super::icon_button(
                        format!("pane-mode-{id}-{label}"),
                        icon,
                        crate::i18n::text(if value == Mode::Shell {
                            "Shell 模式"
                        } else {
                            "Agent 模式"
                        }),
                    )
                    .disabled(mode.is_none());
                    let button = if mode == Some(value) {
                        button
                            .bg(rgb(palette.hover))
                            .text_color(rgb(palette.accent))
                    } else {
                        button
                    };
                    button.on_click(cx.listener(move |view, _, window, cx| {
                        view.set_pane_mode(id, value, window, cx);
                    }))
                }),
            )
            .into_any_element()
    }

    pub(super) fn render_pane_dock(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let palette = self.palette();
        let limit = pane.dock_limit();
        let extent = pane.dock().extent(limit);
        let expanded = extent > 0.;
        let agent = pane.dock.mode() == Mode::Agent;
        let width = f32::from(pane.bounds.get().size.width);
        let compact = agent && width > 0. && width < 460.;
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .min_h_0()
            .min_w_0()
            .h(px(if expanded {
                extent
            } else {
                DOCK_BAR.min(limit)
            }))
            .max_h(gpui_kit::relative(0.5))
            .overflow_hidden()
            .bg(rgb(palette.sidebar))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .h(px(DOCK_BAR))
                    .when(expanded, |header| header.border_b_1())
                    .border_color(rgb(palette.border))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .h_full()
                            .flex_shrink_0()
                            .px_2()
                            .child(
                                crate::controls::button(format!("composer-toggle-{id}"))
                                    .ghost()
                                    .compact()
                                    .icon(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronUp
                                        })
                                        .size_4(),
                                    )
                                    .accessibility_label(crate::i18n::text(if agent {
                                        "AI 助手"
                                    } else {
                                        "命令发送器"
                                    }))
                                    .when(!compact, |button| {
                                        button.label(crate::i18n::text(if agent {
                                            "AI 助手"
                                        } else {
                                            "命令发送器"
                                        }))
                                    })
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.activate_pane(id, cx);
                                        if let Some(pane) = view.panes.get_mut(&id) {
                                            pane.dock_mut().toggle();
                                            if pane.dock_expanded() {
                                                let input = if pane.dock.mode() == Mode::Agent {
                                                    &pane.assistant.prompt
                                                } else {
                                                    &pane.assistant.command
                                                };
                                                input.update(cx, |input, cx| {
                                                    input.focus(window, cx)
                                                });
                                            } else {
                                                pane.view.update(cx, |terminal, cx| {
                                                    terminal.focus(window, cx)
                                                });
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                            .children(
                                (agent && expanded)
                                    .then(|| self.render_agent_configuration(id, cx)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .flex_shrink_0()
                                    .gap_1()
                                    .children(agent.then(|| self.render_agent_controls(id, cx)))
                                    .child(
                                        icon_button(
                                            format!("history-{id}"),
                                            IconName::ClipboardClock,
                                            crate::i18n::text("命令历史"),
                                        )
                                        .on_click(
                                            cx.listener(move |view, _, _, cx| {
                                                view.show_pane_history(id, cx);
                                            }),
                                        ),
                                    ),
                            ),
                    ),
            )
            .children(expanded.then(|| {
                if agent {
                    self.render_agent(id, cx)
                } else {
                    self.render_command_sender(id, cx)
                }
            }))
            .child(panels::handle(
                format!("dock-resize-{id}"),
                PanelEdge::Dock(id),
            ))
            .into_any_element()
    }

    pub(super) fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette();
        let Some((id, pane)) = self
            .active_pane
            .and_then(|id| self.panes.get(&id).map(|pane| (id, pane)))
        else {
            return div()
                .p_4()
                .text_color(rgb(palette.muted))
                .child(crate::i18n::text("选择终端以查看命令历史"))
                .into_any_element();
        };
        let engine = &pane.view.read(cx).terminal.engine;
        let history = engine.command_history();
        let waiting_for_shell = pane.node.is_some() && !engine.shell_integration_active();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .child(
                        div()
                            .text_size(px(12.))
                            .child(pane.view.read(cx).snapshot.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(crate::i18n::text("仅此窗格 · 点击填入命令发送器")),
                    ),
            )
            .child(
                div()
                    .id(format!("command-history-{id}"))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_2()
                    .children(history.is_empty().then(|| {
                        div()
                            .p_3()
                            .text_size(px(12.))
                            .text_color(rgb(palette.muted))
                            .child(crate::i18n::text(if waiting_for_shell {
                                "等待远端 Shell 集成：请在终端按 Enter 返回空提示符"
                            } else {
                                "此会话尚无命令"
                            }))
                    }))
                    .children(history.into_iter().enumerate().map(|(index, entry)| {
                        div()
                            .id(format!("history-command-{id}-{index}"))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .bg(rgb(palette.surface))
                            .hover(|style| style.bg(rgb(palette.hover)))
                            .cursor_pointer()
                            .child(
                                div()
                                    .font(
                                        crate::fonts::FontPair::terminal(
                                            &self.store.profile.preferences,
                                            cx,
                                        )
                                        .latin,
                                    )
                                    .text_size(px(12.))
                                    .child(entry.command.clone()),
                            )
                            .children(entry.directory.map(|directory| {
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(palette.muted))
                                    .text_ellipsis()
                                    .child(directory)
                            }))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.activate_pane(id, cx);
                                if let Some(pane) = view.panes.get_mut(&id) {
                                    pane.dock.set_mode(Mode::Shell);
                                    pane.dock_mut().open = true;
                                    pane.assistant.command.update(cx, |input, cx| {
                                        input.set_value(entry.command.clone(), window, cx);
                                    });
                                    if pane.dock_expanded() {
                                        pane.assistant
                                            .command
                                            .update(cx, |input, cx| input.focus(window, cx));
                                    } else {
                                        pane.view
                                            .update(cx, |terminal, cx| terminal.focus(window, cx));
                                    }
                                }
                                cx.notify();
                            }))
                    })),
            )
            .into_any_element()
    }

    fn render_agent_controls(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let session = &pane.assistant.session;
        let closed = session.ended || pane.view.read(cx).snapshot.closed;
        let read_only = session.auto_read_only;
        let capture = session.capture_output;
        let workspace = cx.entity().downgrade();
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                icon_button(
                    format!("reset-agent-{id}"),
                    IconName::SquarePen,
                    crate::i18n::text("新建会话"),
                )
                .disabled(closed)
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.activate_pane(id, cx);
                    if let Some(pane) = view.panes.get_mut(&id) {
                        pane.assistant.permissions.clear();
                        pane.assistant.session.reset();
                        pane.assistant
                            .prompt
                            .update(cx, |prompt, cx| prompt.set_value("", window, cx));
                    }
                    view.load_agent_configuration(id, false, cx);
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    format!("agent-options-{id}"),
                    IconName::SlidersHorizontal,
                    crate::i18n::text("AI 会话选项"),
                )
                .selected(read_only || capture)
                .disabled(closed)
                .dropdown_menu(move |mut menu, _, _| {
                    menu = menu.check_side(gpui_kit::component::Side::Right);
                    for (index, label, checked, icon) in [
                        (
                            0,
                            "自动发送白名单只读命令",
                            read_only,
                            IconName::ShieldCheck,
                        ),
                        (1, "接管终端输出作为模型上下文", capture, IconName::Terminal),
                    ] {
                        let workspace = workspace.clone();
                        menu = menu.item(
                            PopupMenuItem::new(crate::i18n::text(label))
                                .icon(icon)
                                .checked(checked)
                                .on_click(move |_, _, cx| {
                                    let _ = workspace.update(cx, |view, cx| {
                                        view.activate_pane(id, cx);
                                        if let Some(pane) = view.panes.get_mut(&id)
                                            && !pane.assistant.session.ended
                                        {
                                            let session = &mut pane.assistant.session;
                                            if index == 0 {
                                                session.auto_read_only = !session.auto_read_only;
                                            } else {
                                                session.capture_output = !session.capture_output;
                                                if !session.capture_output {
                                                    session.stop();
                                                }
                                            }
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

    pub(super) fn send_composed_command(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_pane(id, cx);
        if let Some(pane) = self.panes.get_mut(&id) {
            let text = pane.assistant.command.read(cx).value().to_string();
            match pane.view.read(cx).terminal.engine.send_command(&text) {
                Ok(()) => {
                    pane.assistant.command_error = None;
                    self.status = None;
                    pane.view
                        .update(cx, |terminal, cx| terminal.focus(window, cx));
                }
                Err(error) => {
                    pane.assistant.command_error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    pub(super) fn ask_pane(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant_locked(id, cx) {
            return;
        }
        self.activate_pane(id, cx);
        let agent = self
            .panes
            .get(&id)
            .and_then(|pane| pane.assistant.provider.read(cx).selected_value())
            .and_then(|name| name.strip_prefix("ACP: "))
            .and_then(|name| {
                self.store
                    .profile
                    .agents
                    .iter()
                    .find(|agent| agent.enabled && agent.name == name)
            })
            .cloned();
        let selection = match if agent.is_some() {
            Ok(Selection {
                provider: Provider::legacy(&self.store.profile.preferences),
                model: self.store.profile.preferences.ai_model.clone(),
                reasoning: Reasoning::Auto,
                proxy: None,
            })
        } else {
            self.pane_selection(id, cx)
        } {
            Ok(selection) => selection,
            Err(error) => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.assistant.session.error = Some(error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let snapshot = pane.view.read(cx).terminal.engine.snapshot();
        if snapshot.closed {
            pane.assistant.session.end();
            cx.notify();
            return;
        }
        let prompt = pane.assistant.prompt.read(cx).value().to_string();
        let (request, cancel) = match pane
            .assistant
            .session
            .begin(prompt, snapshot.automation_revision)
        {
            Ok(pending) => pending,
            Err(error) => {
                pane.assistant.session.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let session = pane.assistant.session.id;
        let conversation = pane.assistant.session.conversation();
        let output = pane
            .assistant
            .session
            .capture_output
            .then(|| pane.view.read(cx).terminal.engine.recent_output());
        pane.assistant
            .prompt
            .update(cx, |prompt, cx| prompt.set_value("", window, cx));
        pane.assistant.prompt_expanded = false;
        let (permissions, receive_permissions) = async_channel::bounded(8);
        let session_cancel = pane.assistant.session.runtime.cancel.clone();
        let permission_events = self.events.clone();
        let (progress, progress_receiver) = async_channel::bounded(1);
        let progress_cancel = cancel.clone();
        let request_data = ai::Request {
            selection,
            agent,
            permissions: Some(permissions),
            progress: Some(progress),
            conversation,
            context: ai::TerminalContext {
                environment: pane.view.read(cx).terminal.engine.environment(),
                output,
            },
            cancel,
            runtime: pane.assistant.session.runtime.clone(),
            servers: self.store.profile.mcp_servers.clone(),
            proxies: self.store.profile.proxies.clone(),
            roots: self.store.profile.skills_roots.clone(),
        };
        self.launch(async move{loop{tokio::select!{()=session_cancel.cancelled()=>break,result=receive_permissions.recv()=>{let Ok(permission)=result else{break;};if permission_events.send(BackendEvent::AgentPermission{pane:id,session,permission}).await.is_err(){break;}}}}});
        let progress_events = self.events.clone();
        self.launch(async move {
            loop {
                let text = tokio::select! { () = progress_cancel.cancelled() => break, result = progress_receiver.recv() => { let Ok(text) = result else { break; }; text } };
                if progress_events.send(BackendEvent::AiProgress { pane: id, session, request, text }).await.is_err() { break; }
            }
        });
        let events = self.events.clone();
        self.launch(async move {
            let result = ai::ask(request_data).await;
            let _ = events
                .send(BackendEvent::Ai {
                    pane: id,
                    session,
                    request,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    pub(super) fn receive_ai(
        &mut self,
        id: Uuid,
        session: Uuid,
        request: Uuid,
        result: Result<ai::Reply>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let engine = &pane.view.read(cx).terminal.engine;
        if pane.view.read(cx).snapshot.closed {
            pane.assistant.session.end();
        }
        let completed = result.is_ok();
        let Some(generation) = pane.assistant.session.accept(session, request, result) else {
            return;
        };
        if !completed || !pane.assistant.session.auto_read_only {
            return;
        }
        let ready =
            engine.automatic_command_ready(generation) && (pane.node.is_some() || !cfg!(windows));
        let Some(entry) = pane.assistant.session.entries.last_mut() else {
            return;
        };
        let state = if ready {
            crate::assistant_session::AutomationState::Ready
        } else if !engine.shell_integration_active() {
            crate::assistant_session::AutomationState::ShellUnavailable
        } else {
            crate::assistant_session::AutomationState::TerminalChanged
        };
        let dispatch = entry.prepare_automatic(&engine.environment(), state);
        if !dispatch.is_empty() {
            let commands = dispatch
                .iter()
                .map(|(_, command)| command.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            let result = engine.send_automatic_command(&commands, generation);
            for (id, _) in dispatch {
                if let Some(command) = pane.assistant.session.command_mut(id) {
                    command.state = match &result {
                        Ok(()) => CommandState::Sent,
                        Err(error) => CommandState::Deferred(error.to_string()),
                    };
                }
            }
        }
    }

    fn execute_proposal(
        &mut self,
        id: Uuid,
        session: Uuid,
        command: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_pane(id, cx);
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.assistant.session.id != session || pane.assistant.session.ended {
            return;
        }
        let Some(proposal) = pane.assistant.session.command_mut(command) else {
            return;
        };
        let environment = pane.view.read(cx).terminal.engine.environment();
        if proposal
            .language
            .as_deref()
            .is_some_and(|language| !command::language_supported(language, &environment))
        {
            proposal.state = CommandState::Deferred(
                crate::i18n::text("命令语言与当前终端 Shell 不匹配，已阻止发送").into(),
            );
            cx.notify();
            return;
        }
        proposal.state = match pane
            .view
            .read(cx)
            .terminal
            .engine
            .send_command(&proposal.text)
        {
            Ok(()) => CommandState::Sent,
            Err(error) => CommandState::Deferred(error.to_string()),
        };
        if proposal.state == CommandState::Sent {
            pane.view
                .update(cx, |terminal, cx| terminal.focus(window, cx));
        }
        cx.notify();
    }

    fn invoke_tool(&mut self, id: Uuid, session: Uuid, tool_id: Uuid, cx: &mut Context<Self>) {
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.assistant.session.id != session || pane.view.read(cx).snapshot.closed {
            return;
        }
        let cancel = match pane.assistant.session.begin_tool(tool_id) {
            Ok(cancel) => cancel,
            Err(error) => {
                pane.assistant.session.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let Some(tool) = pane
            .assistant
            .session
            .entries
            .iter()
            .flat_map(|entry| &entry.tools)
            .find(|tool| tool.id == tool_id)
            .cloned()
        else {
            return;
        };
        let runtime = pane.assistant.session.runtime.clone();
        let roots = self.store.profile.skills_roots.clone();
        let events = self.events.clone();
        self.launch(async move {
            let result = runtime
                .invoke(
                    &tool.target,
                    &serde_json::Value::Object(tool.call.function.arguments.clone()),
                    &roots,
                    cancel,
                )
                .await;
            let _ = events
                .send(BackendEvent::ToolResult {
                    pane: id,
                    session,
                    tool: tool_id,
                    result,
                })
                .await;
        });
        cx.notify();
    }
    fn reject_tool(&mut self, id: Uuid, session: Uuid, tool: Uuid, cx: &mut Context<Self>) {
        let accepted = self.panes.get_mut(&id).is_some_and(|pane| {
            pane.assistant.session.id == session && pane.assistant.session.reject_tool(tool)
        });
        if accepted {
            self.continue_tool_reply(id, cx);
        }
        cx.notify();
    }
    pub(super) fn receive_tool(
        &mut self,
        id: Uuid,
        session: Uuid,
        tool: Uuid,
        result: Result<String>,
        cx: &mut Context<Self>,
    ) {
        let accepted = self
            .panes
            .get_mut(&id)
            .is_some_and(|pane| pane.assistant.session.finish_tool(session, tool, result));
        if accepted {
            self.continue_tool_reply(id, cx);
        }
    }
    fn continue_tool_reply(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let selection = match self.pane_selection(id, cx) {
            Ok(selection) => selection,
            Err(error) => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.assistant.session.error = Some(error.to_string());
                }
                return;
            }
        };
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.assistant.session.unresolved_tools() || pane.view.read(cx).snapshot.closed {
            return;
        }
        let (request, cancel) = match pane
            .assistant
            .session
            .continue_tools(pane.view.read(cx).snapshot.automation_revision)
        {
            Ok(value) => value,
            Err(error) => {
                pane.assistant.session.error = Some(error.to_string());
                return;
            }
        };
        let session = pane.assistant.session.id;
        let (progress, progress_receiver) = async_channel::bounded(1);
        let progress_cancel = cancel.clone();
        let request_data = ai::Request {
            selection,
            agent: None,
            permissions: None,
            progress: Some(progress),
            conversation: pane.assistant.session.conversation(),
            context: ai::TerminalContext {
                environment: pane.view.read(cx).terminal.engine.environment(),
                output: pane
                    .assistant
                    .session
                    .capture_output
                    .then(|| pane.view.read(cx).terminal.engine.recent_output()),
            },
            cancel,
            runtime: pane.assistant.session.runtime.clone(),
            servers: self.store.profile.mcp_servers.clone(),
            proxies: self.store.profile.proxies.clone(),
            roots: self.store.profile.skills_roots.clone(),
        };
        let progress_events = self.events.clone();
        self.launch(async move {
            loop {
                let text = tokio::select! { () = progress_cancel.cancelled() => break, result = progress_receiver.recv() => { let Ok(text) = result else { break; }; text } };
                if progress_events.send(BackendEvent::AiProgress { pane: id, session, request, text }).await.is_err() { break; }
            }
        });
        let events = self.events.clone();
        self.launch(async move {
            let result = ai::ask(request_data).await;
            let _ = events
                .send(BackendEvent::Ai {
                    pane: id,
                    session,
                    request,
                    result,
                })
                .await;
        });
    }
    fn render_tool_cards(
        &self,
        id: Uuid,
        session: Uuid,
        tools: &[crate::integrations::ProposedTool],
        closed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::integrations::{Target, ToolState};
        let p = self.palette();
        let busy = self.panes.get(&id).is_some_and(|pane| {
            pane.assistant.session.pending.is_some() || pane.assistant.session.tool_running()
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .children(tools.iter().map(|tool| {
                let tool_id = tool.id;
                let suggested = tool.state == ToolState::Suggested;
                let name = match &tool.target {
                    Target::Mcp { server, name } => format!(
                        "{} / {name}",
                        self.store
                            .profile
                            .mcp_servers
                            .iter()
                            .find(|item| item.id == *server)
                            .map(|item| item.name.as_str())
                            .unwrap_or("MCP")
                    ),
                    Target::ListSkills => crate::i18n::text("列出 Skills").into(),
                    Target::LoadSkill => crate::i18n::text("加载 Skill").into(),
                    Target::ReadSkillResource => crate::i18n::text("读取 Skill 资源").into(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.base))
                    .child(name)
                    .child(
                        div()
                            .text_size(px(11.))
                            .font(
                                crate::fonts::FontPair::agent(&self.store.profile.preferences, cx)
                                    .latin,
                            )
                            .child(
                                serde_json::to_string_pretty(&tool.call.function.arguments)
                                    .unwrap_or_default(),
                            ),
                    )
                    .child(div().text_size(px(11.)).text_color(rgb(p.muted)).child(
                        match &tool.state {
                            ToolState::Suggested => {
                                crate::i18n::text("待确认 · 执行前请检查参数").into()
                            }
                            ToolState::Running => crate::i18n::text("正在执行…").into(),
                            ToolState::Completed => crate::i18n::text("已完成").into(),
                            ToolState::Rejected => crate::i18n::text("已拒绝").into(),
                            ToolState::Failed(error) => error.clone(),
                        },
                    ))
                    .children(suggested.then(|| {
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                crate::controls::button(format!("reject-tool-{tool_id}"))
                                    .ghost()
                                    .compact()
                                    .label(crate::i18n::text("拒绝"))
                                    .disabled(closed || busy)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.reject_tool(id, session, tool_id, cx)
                                    })),
                            )
                            .child(
                                crate::controls::button(format!("invoke-tool-{tool_id}"))
                                    .primary()
                                    .compact()
                                    .label(crate::i18n::text("执行工具"))
                                    .disabled(closed || busy)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.invoke_tool(id, session, tool_id, cx)
                                    })),
                            )
                    }))
            }))
            .into_any_element()
    }

    fn answer_agent_permission(
        &mut self,
        id: Uuid,
        session: Uuid,
        permission: Uuid,
        answer: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.panes.get_mut(&id)
            && pane.assistant.session.id == session
            && let Some(index) = pane
                .assistant
                .permissions
                .iter()
                .position(|item| item.id == permission)
        {
            let permission = pane.assistant.permissions.remove(index);
            if !permission.cancel.is_cancelled() {
                let _ = permission.answer.send(answer);
            }
        }
        cx.notify();
    }
    pub(super) fn assistant_locked(&self, id: Uuid, cx: &gpui_kit::App) -> bool {
        self.panes.get(&id).is_none_or(|pane| {
            let session = &pane.assistant.session;
            session.ended
                || pane.view.read(cx).snapshot.closed
                || session.pending.is_some()
                || session.tool_running()
                || pane.assistant.agent_settings.changing(session.id)
        })
    }

    pub(super) fn assistant_target(
        &self,
        id: Uuid,
        cx: &gpui_kit::App,
    ) -> Option<super::model_picker::Target> {
        let pane = self.panes.get(&id)?;
        let name = pane.assistant.provider.read(cx).selected_value()?;
        if let Some(name) = name.strip_prefix("ACP: ") {
            return self
                .store
                .profile
                .agents
                .iter()
                .find(|agent| agent.enabled && agent.name == name)
                .map(|agent| super::model_picker::Target::Agent(agent.id));
        }
        let provider = self
            .store
            .profile
            .ai_providers
            .iter()
            .find(|provider| provider.enabled && provider.name == name.as_ref())?;
        Some(super::model_picker::Target::Model {
            provider: provider.id,
            model: pane.assistant.model.read(cx).selected_value()?.to_string(),
        })
    }

    pub(super) fn select_assistant_target(
        &mut self,
        id: Uuid,
        target: super::model_picker::Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let super::model_picker::Target::AgentValue {
            agent,
            config,
            value,
        } = target
        {
            self.choose_agent_model(id, agent, config, value, window, cx);
            return;
        }
        if self.assistant_locked(id, cx) || self.assistant_target(id, cx).as_ref() == Some(&target)
        {
            return;
        }
        tracing::debug!(pane = %id, ?target, "assistant model selected");
        let (name, models, model) = match target {
            super::model_picker::Target::Model { provider, model } => {
                let Some(provider) = self.store.profile.ai_providers.iter().find(|p| {
                    p.id == provider && p.enabled && p.models.iter().any(|m| m.id == model)
                }) else {
                    return;
                };
                (
                    provider.name.clone(),
                    provider
                        .models
                        .iter()
                        .map(|m| SharedString::from(m.id.clone()))
                        .collect::<Vec<_>>(),
                    model,
                )
            }
            super::model_picker::Target::Agent(id) => {
                let Some(agent) = self
                    .store
                    .profile
                    .agents
                    .iter()
                    .find(|agent| agent.id == id && agent.enabled)
                else {
                    return;
                };
                (
                    format!("ACP: {}", agent.name),
                    vec![SharedString::from("Agent default")],
                    "Agent default".into(),
                )
            }
            super::model_picker::Target::AgentValue { .. } => return,
        };
        self.activate_pane(id, cx);
        if let Some(pane) = self.panes.get_mut(&id) {
            let provider_changed = pane
                .assistant
                .provider
                .read(cx)
                .selected_value()
                .is_none_or(|previous| previous.as_ref() != name);
            if provider_changed {
                pane.assistant.session.reset();
                pane.assistant.permissions.clear();
            } else {
                pane.assistant.session.stop();
            }
            pane.assistant.provider.update(cx, |state, cx| {
                state.set_selected_value(&SharedString::from(name), window, cx)
            });
            pane.assistant.model.update(cx, |state, cx| {
                state.set_items(models.into(), window, cx);
                state.set_selected_value(&SharedString::from(model), window, cx);
            });
        }
        self.refresh_reasoning(id, window, cx);
        self.load_agent_configuration(id, false, cx);
        cx.notify();
    }

    pub(super) fn select_assistant_reasoning(
        &mut self,
        id: Uuid,
        level: Reasoning,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.assistant_locked(id, cx) {
            return;
        }
        let valid = self
            .pane_selection(id, cx)
            .ok()
            .and_then(|selection| {
                selection
                    .provider
                    .models
                    .iter()
                    .find(|model| model.id == selection.model)
                    .map(|model| Reasoning::available(&model.reasoning).contains(&level))
            })
            .unwrap_or(false);
        if !valid {
            return;
        }
        tracing::debug!(pane = %id, ?level, "assistant reasoning selected");
        self.activate_pane(id, cx);
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.assistant.reasoning.update(cx, |state, cx| {
                state.set_selected_value(&SharedString::from(level.display_label()), window, cx)
            });
        }
        cx.notify();
    }

    fn render_agent_configuration(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let palette = self.palette();
        let model = pane
            .assistant
            .model
            .read(cx)
            .selected_value()
            .map(|name| name.to_string())
            .unwrap_or_else(|| crate::i18n::text("选择模型").into());
        let label = match self.assistant_target(id, cx) {
            Some(super::model_picker::Target::Agent(agent)) => self
                .store
                .profile
                .agents
                .iter()
                .find(|candidate| candidate.id == agent)
                .map(|agent| {
                    if pane.assistant.agent_settings.session == pane.assistant.session.id
                        && pane.assistant.agent_settings.agent == Some(agent.id)
                        && let Some(model) = pane.assistant.agent_settings.configuration.model()
                        && !model.current_label().is_empty()
                    {
                        format!("{} · {}", agent.name, model.current_label())
                    } else {
                        agent.name.clone()
                    }
                })
                .unwrap_or(model),
            _ => model,
        };
        let picker = pane.assistant.picker.clone();
        let focus = picker.read(cx).focus_handle(cx);
        let on_open = picker.clone();
        let workspace = cx.entity().downgrade();
        let trigger = crate::controls::button(format!("model-picker-{id}"))
            .ghost()
            .compact()
            .min_w_0()
            .max_w(gpui_kit::relative(1.))
            .flex_shrink_1()
            .h(px(28.))
            .disabled(self.assistant_locked(id, cx))
            .accessibility_label(crate::i18n::text("选择模型与推理强度"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .gap_1()
                    .child(div().min_w_0().text_ellipsis().child(label))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size_3()
                            .flex_shrink_0()
                            .text_color(rgb(palette.muted)),
                    ),
            );
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .min_w_0()
            .max_w(px(240.))
            .child(
                Popover::new(format!("model-popover-{id}"))
                    .anchor(gpui_kit::Anchor::BottomRight)
                    .track_focus(&focus)
                    .trigger_style(
                        div()
                            .min_w_0()
                            .max_w(gpui_kit::relative(1.))
                            .style()
                            .clone(),
                    )
                    .trigger(trigger)
                    .on_open_change(move |open, window, cx| {
                        if *open {
                            on_open.update(cx, |picker, cx| picker.reset(window, cx));
                            let _ = workspace.update(cx, |view, cx| {
                                view.load_agent_configuration(id, false, cx)
                            });
                        }
                    })
                    .content(move |_, _, cx| {
                        let popover = cx.entity().downgrade();
                        picker.update(cx, |picker, _| picker.popover = Some(popover));
                        picker.clone()
                    }),
            )
            .into_any_element()
    }

    fn restore_user_message(
        &mut self,
        id: Uuid,
        session: Uuid,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.assistant_locked(id, cx) {
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        let Some(text) = pane
            .assistant
            .session
            .user_message(session, index)
            .map(str::to_owned)
        else {
            return;
        };
        pane.assistant.prompt.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.focus(window, cx);
        });
        pane.dock_mut().open = true;
        self.activate_pane(id, cx);
        cx.notify();
    }

    pub(super) fn render_agent(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[&id];
        let session = &pane.assistant.session;
        let session_id = session.id;
        let palette = self.palette();
        let fonts = crate::fonts::FontPair::agent(&self.store.profile.preferences, cx);
        let size = self.store.profile.preferences.agent_font_size;
        let closed = session.ended || pane.view.read(cx).snapshot.closed;
        let auto_label = if !session.auto_read_only {
            "只读自动发送：关"
        } else if !pane
            .view
            .read(cx)
            .terminal
            .engine
            .shell_integration_active()
        {
            "只读自动发送：等待 Shell"
        } else {
            "只读自动发送：开"
        };
        let input_expanded =
            pane.assistant.prompt_expanded && session.pending.is_none() && !session.tool_running();
        div().id(format!("agent-surface-{id}")).flex().flex_col().flex_1().min_h_0().min_w_0().bg(rgb(palette.base)).font(fonts.latin.clone()).text_size(px(size))
            .child(div().flex().items_center().justify_end().gap_2().flex_shrink_0().min_w_0().px_3().h(px(24.)).text_size(px(11.))
                .child(div().flex().min_w_0().items_center().gap_1().text_color(rgb(if session.auto_read_only {palette.accent} else {palette.muted}))
                    .child(Icon::new(if session.auto_read_only {IconName::ShieldCheck} else {IconName::Shield}).size_3())
                    .child(div().min_w_0().text_ellipsis().child(crate::i18n::text(auto_label))))
                .child(div().flex().min_w_0().items_center().gap_1().text_color(rgb(if session.capture_output {palette.accent} else {palette.muted}))
                    .child(Icon::new(IconName::Terminal).size_3())
                    .child(div().min_w_0().text_ellipsis().child(crate::i18n::text(if session.capture_output {"终端上下文：开"} else {"终端上下文：关"})))))
            .children((!input_expanded).then(|| div().id(format!("agent-conversation-{id}")).flex().flex_col().flex_1().min_h_0().overflow_y_scroll().px_3().py_2().gap_2()
            .children(pane.assistant.permissions.iter().filter(|permission|!permission.cancel.is_cancelled()).map(|permission|{
                let permission_id=permission.id;
                div().flex().flex_col().gap_2().p_3().border_b_1().border_color(rgb(palette.border)).child(crate::i18n::text("Agent 请求执行工具")).child(div().text_size(px(11.)).child(permission.detail.clone()))
                    .child(div().flex().gap_2().children(permission.options.iter().enumerate().map(|(index,label)|crate::controls::button(format!("acp-permission-{permission_id}-{index}")).compact().label(label.clone()).on_click(cx.listener(move|view,_,_,cx|view.answer_agent_permission(id,session_id,permission_id,Some(index),cx))))).child(crate::controls::button(format!("acp-cancel-{permission_id}")).ghost().compact().label(crate::i18n::text("拒绝")).on_click(cx.listener(move|view,_,_,cx|view.answer_agent_permission(id,session_id,permission_id,None,cx)))))
            }))
                .children(session.entries.is_empty().then(|| div().flex().flex_col().gap_2().py_1()
                    .child(div().text_size(px(14.)).child(crate::i18n::text("从这个终端开始")))
                    .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(crate::i18n::text("询问命令或排查问题。命令卡片可直接发送到此窗格，切换会话不会混入其他终端的内容。")))))
                .children(session.entries.iter().enumerate().map(|(index, entry)| {
                    div().flex().flex_col().gap_2().p_2().rounded_lg().bg(rgb(if entry.turn.is_user() { palette.hover } else { palette.surface }))
                        .child(div().flex().items_center().justify_between().min_w_0().text_size(px(10.)).text_color(rgb(palette.muted))
                            .child(if entry.turn.is_user() { crate::i18n::text("你") } else { "Nutty" })
                            .children(entry.turn.is_user().then(|| crate::controls::button(format!("reuse-message-{id}-{session_id}-{index}"))
                                .ghost().compact().w(px(24.)).h(px(24.)).icon(Icon::new(IconName::Copy).size_4())
                                .accessibility_label(crate::i18n::text("复制到输入框")).tooltip(crate::i18n::text("复制到输入框"))
                                .disabled(closed || self.assistant_locked(id, cx))
                                .on_click(cx.listener(move |view, _, window, cx| view.restore_user_message(id, session_id, index, window, cx))))))
                        .child(self.render_reply(id, session_id, index, entry, closed, cx))
                        .child(self.render_tool_cards(id, session_id, &entry.tools, closed, cx))
                        .into_any_element()
                }))
                .children(session.pending.as_ref().filter(|pending| !pending.partial.is_empty()).map(|pending| {
                    div().p_2().rounded_lg().bg(rgb(palette.surface))
                        .child(TextView::markdown(format!("agent-stream-{id}-{}", pending.id), pending.partial.clone()).font(fonts.latin.clone()).text_size(px(size)).style(self.agent_text_style(cx)))
                }))))
            .children(session.error.as_ref().map(|error| self.render_agent_error(id, error, closed, cx)))
            .child(self.render_agent_input(id, cx))
            .into_any_element()
    }

    fn agent_text_style(&self, cx: &Context<Self>) -> TextViewStyle {
        let font = crate::fonts::FontPair::agent(&self.store.profile.preferences, cx).latin;
        TextViewStyle::default()
            .paragraph_gap(gpui_kit::rems(0.45))
            .heading_font_size(|level, base| base * if level <= 2 { 1.15 } else { 1. })
            .code_block(
                gpui_kit::StyleRefinement::default()
                    .font(font)
                    .text_size(px(self.store.profile.preferences.agent_font_size))
                    .pt(px(42.))
                    .pb(px(8.))
                    .px(px(10.)),
            )
    }

    fn render_reply(
        &self,
        id: Uuid,
        session: Uuid,
        index: usize,
        entry: &crate::assistant_session::Entry,
        closed: bool,
        cx: &mut Context<Self>,
    ) -> TextView {
        let proposals = entry.commands.clone();
        let environment = self.panes[&id].view.read(cx).terminal.engine.environment();
        let workspace = cx.entity().downgrade();
        let palette = self.palette();
        TextView::markdown(
            format!("agent-message-{id}-{session}-{index}"),
            entry.turn.text().to_string(),
        )
        .font(crate::fonts::FontPair::agent(&self.store.profile.preferences, cx).latin)
        .text_size(px(self.store.profile.preferences.agent_font_size))
        .style(self.agent_text_style(cx))
        .code_block_actions(move |block, _, _| {
            let code = block.code();
            let language = block.lang();
            let proposal = proposals
                .iter()
                .find(|proposal| proposal.matches_block(language.as_deref(), code.as_ref()));
            let copy_code = code.to_string();
            let mut actions = div()
                .flex()
                .items_center()
                .gap_1()
                .text_color(rgb(palette.muted))
                .children(language.map(|language| div().text_size(px(11.)).mr_1().child(language)))
                .child(
                    icon_button("copy-code", IconName::Copy, crate::i18n::text("复制代码"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                copy_code.clone(),
                            ));
                        }),
                );
            if let Some(proposal) = proposal {
                let incompatible = proposal
                    .language
                    .as_deref()
                    .is_some_and(|language| !command::language_supported(language, &environment));
                let deferred_reason = if incompatible {
                    Some(crate::i18n::text(
                        "命令语言与当前终端 Shell 不匹配，已阻止发送",
                    ))
                } else if let CommandState::Deferred(reason) = &proposal.state {
                    Some(reason.as_str())
                } else {
                    None
                };
                if let Some(reason) = deferred_reason {
                    let detail = reason.to_owned();
                    actions = actions.child(
                        div()
                            .id(format!("command-deferred-{}", proposal.id))
                            .min_w_0()
                            .max_w(px(180.))
                            .text_size(px(10.))
                            .text_ellipsis()
                            .child(reason.to_owned())
                            .tooltip(move |_, cx| {
                                cx.new(|_| {
                                    gpui_kit::component::tooltip::Tooltip::new(detail.clone())
                                })
                                .into()
                            }),
                    );
                }
                let command = proposal.id;
                let sent = proposal.state == CommandState::Sent;
                let group = SharedString::from(format!("repeat-command-{command}"));
                let workspace = workspace.clone();
                actions = actions.child(
                    crate::controls::button("execute-code")
                        .group(group.clone())
                        .ghost()
                        .compact()
                        .icon(
                            Icon::new(if sent {
                                IconName::Check
                            } else {
                                IconName::Play
                            })
                            .size_4(),
                        )
                        .accessibility_label(crate::i18n::text(if sent {
                            "重新发送"
                        } else {
                            "执行"
                        }))
                        .child(
                            div()
                                .relative()
                                .w(px(if sent { 62. } else { 30. }))
                                .child(
                                    div()
                                        .child(crate::i18n::text(if sent {
                                            "已发送"
                                        } else {
                                            "执行"
                                        }))
                                        .when(sent, |label| {
                                            label.group_hover(group.clone(), |style| {
                                                style.opacity(0.)
                                            })
                                        }),
                                )
                                .children(sent.then(|| {
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .opacity(0.)
                                        .group_hover(group, |style| style.opacity(1.))
                                        .child(crate::i18n::text("重新发送"))
                                })),
                        )
                        .tooltip(deferred_reason.map_or_else(
                            || {
                                crate::i18n::text(if sent {
                                    "重新发送到当前终端"
                                } else {
                                    "待发送 · 此窗格"
                                })
                                .to_owned()
                            },
                            str::to_owned,
                        ))
                        .disabled(closed || incompatible)
                        .on_click(move |_, window, cx| {
                            let _ = workspace.update(cx, |view, cx| {
                                view.execute_proposal(id, session, command, window, cx);
                            });
                        }),
                );
            }
            actions
        })
    }

    fn render_agent_error(
        &self,
        id: Uuid,
        error: &str,
        closed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = &self.panes[&id];
        let target = self.assistant_target(id, cx);
        let needs_login = pane.assistant.session.needs_agent_login
            || self.store.profile.agents.iter().any(|agent| {
                target == Some(super::model_picker::Target::Agent(agent.id))
                    && agent.process.supports_terminal_login()
            });
        let is_agent = matches!(target, Some(super::model_picker::Target::Agent(_)));
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .py_2()
            .text_size(px(12.))
            .child(
                div().text_color(rgb(0xf17c82)).child(
                    error
                        .split(": ")
                        .map(|part| crate::i18n::localize(part.to_string()))
                        .collect::<Vec<_>>()
                        .join(": "),
                ),
            )
            .children(is_agent.then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(needs_login.then(|| {
                        crate::controls::button(format!("login-agent-{id}"))
                            .ghost()
                            .compact()
                            .icon(Icon::new(IconName::Terminal).size_4())
                            .label(crate::i18n::text("准备登录命令"))
                            .disabled(closed)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.prepare_agent_login(id, window, cx);
                            }))
                    }))
                    .child(
                        crate::controls::button(format!("retry-agent-{id}"))
                            .ghost()
                            .compact()
                            .label(crate::i18n::text("重试连接"))
                            .disabled(self.assistant_locked(id, cx))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.retry_assistant(id, window, cx);
                            })),
                    )
            }))
            .children(pane.assistant.session.needs_credentials.then(|| {
                crate::controls::button(format!("retry-keychain-{id}"))
                    .ghost()
                    .compact()
                    .icon(Icon::new(IconName::RefreshCw).size_4())
                    .label(crate::i18n::text("重试钥匙串"))
                    .tooltip(crate::i18n::text(
                        "解锁系统钥匙串后，重新授权并重试刚才的问题",
                    ))
                    .disabled(closed || self.assistant_locked(id, cx))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.retry_assistant(id, window, cx);
                    }))
            }))
            .children(
                (!is_agent && pane.assistant.session.needs_credentials).then(|| {
                    let provider = self
                        .pane_selection(id, cx)
                        .ok()
                        .map(|selection| selection.provider.id);
                    crate::controls::button(format!("repair-provider-key-{id}"))
                        .ghost()
                        .compact()
                        .icon(Icon::new(IconName::Settings).size_4())
                        .label(crate::i18n::text("配置提供方密钥"))
                        .disabled(closed || provider.is_none())
                        .on_click(cx.listener(move |view, _, window, cx| {
                            if let Some(provider) = provider {
                                view.activate_pane(id, cx);
                                view.edit_record(
                                    super::settings::SettingsPage::Providers,
                                    Some(provider),
                                    window,
                                    cx,
                                );
                            }
                        }))
                }),
            )
            .into_any_element()
    }

    fn prepare_agent_login(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(super::model_picker::Target::Agent(agent_id)) = self.assistant_target(id, cx)
        else {
            return;
        };
        let command = self
            .store
            .profile
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .and_then(|agent| agent.process.login_command());
        let Some(command) = command else {
            self.status =
                Some(crate::i18n::text("请按 Agent 的说明在本地终端登录，再重试连接").into());
            cx.notify();
            return;
        };
        self.activate_pane(id, cx);
        if self.panes.get(&id).is_some_and(|pane| pane.node.is_some()) {
            self.local(None, window, cx);
        }
        if let Some(pane) = self.active_pane.and_then(|id| self.panes.get_mut(&id))
            && pane.node.is_none()
            && !pane.view.read(cx).snapshot.closed
        {
            pane.dock.set_mode(Mode::Shell);
            pane.dock_mut().open = true;
            pane.assistant.command.update(cx, |input, cx| {
                input.set_value(command, window, cx);
                input.focus(window, cx);
            });
            self.status = Some(
                crate::i18n::text("登录命令已填入命令发送器，执行并完成登录后返回 Agent 重试")
                    .into(),
            );
        }
        cx.notify();
    }

    fn retry_assistant(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.assistant_locked(id, cx) {
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.assistant.session.needs_credentials {
            crate::config::retry_secret_access();
        }
        let Some(prompt) = pane
            .assistant
            .session
            .entries
            .iter()
            .rev()
            .find(|entry| entry.turn.is_user())
            .map(|entry| entry.turn.text().to_string())
        else {
            self.load_agent_configuration(id, true, cx);
            return;
        };
        if pane
            .assistant
            .session
            .entries
            .last()
            .is_some_and(|entry| entry.turn.is_user())
        {
            pane.assistant.session.entries.pop();
        }
        pane.assistant
            .prompt
            .update(cx, |input, cx| input.set_value(prompt, window, cx));
        self.ask_pane(id, window, cx);
    }
}
