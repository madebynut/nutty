//! Compact, searchable provider-grouped model menu for every pane width.

use gpui_kit::assets::IconName;
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::{
    Icon,
    button::ButtonVariants as _,
    input::{InputEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    popover::PopoverState,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable, IntoElement, Render, Subscription,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::*, px, rgb, uniform_list,
};
use uuid::Uuid;

use super::{WorkspaceView, settings::SettingsPage};
use crate::{
    integrations::Agent,
    provider::{Provider, Reasoning},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Model {
        provider: Uuid,
        model: String,
    },
    Agent(Uuid),
    AgentValue {
        agent: Uuid,
        config: String,
        value: String,
    },
}

#[derive(Clone)]
enum Entry {
    Heading(String),
    Choice { target: Target, label: String },
}

fn agent_option_label(category: &crate::acp::configuration::Category, label: &str) -> String {
    if *category == crate::acp::configuration::Category::Reasoning {
        if label == "On" {
            return crate::i18n::text("开启").to_string();
        }
        if let Some(level) = Reasoning::from_display_label(label) {
            return level.display_label().to_string();
        }
    }
    label.to_string()
}

fn entries(providers: &[Provider], agents: &[Agent], query: &str) -> Vec<Entry> {
    let query = query.to_lowercase();
    let tokens: Vec<_> = query.split_whitespace().collect();
    let mut entries = Vec::new();
    for provider in providers.iter().filter(|provider| provider.enabled) {
        let models: Vec<_> = provider
            .models
            .iter()
            .filter(|model| {
                let search = format!("{} {} {}", provider.name, provider.kind.label(), model.id)
                    .to_lowercase();
                tokens.iter().all(|token| search.contains(token))
            })
            .collect();
        if models.is_empty() {
            continue;
        }
        entries.push(Entry::Heading(provider.name.clone()));
        entries.extend(models.into_iter().map(|model| Entry::Choice {
            target: Target::Model {
                provider: provider.id,
                model: model.id.clone(),
            },
            label: model.id.clone(),
        }));
    }
    let agents: Vec<_> = agents
        .iter()
        .filter(|agent| {
            agent.enabled
                && tokens
                    .iter()
                    .all(|token| format!("ACP {}", agent.name).to_lowercase().contains(token))
        })
        .collect();
    if !agents.is_empty() {
        entries.push(Entry::Heading("ACP Agents".into()));
        entries.extend(agents.into_iter().map(|agent| Entry::Choice {
            target: Target::Agent(agent.id),
            label: agent.name.clone(),
        }));
    }
    entries
}

fn agent_entries(
    agent: &Agent,
    configuration: &crate::acp::configuration::Configuration,
    query: &str,
) -> Vec<Entry> {
    let Some(model) = configuration.model() else {
        return Vec::new();
    };
    let query = query.to_lowercase();
    let tokens: Vec<_> = query.split_whitespace().collect();
    let mut entries = Vec::new();
    let mut heading = None;
    for choice in &model.choices {
        let search = format!(
            "{} {} {} {}",
            agent.name,
            choice.label,
            choice.value,
            choice.group.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        if !tokens.iter().all(|token| search.contains(token)) {
            continue;
        }
        let group = choice.group.clone().unwrap_or_else(|| agent.name.clone());
        if heading.as_ref() != Some(&group) {
            entries.push(Entry::Heading(group.clone()));
            heading = Some(group);
        }
        entries.push(Entry::Choice {
            target: Target::AgentValue {
                agent: agent.id,
                config: model.id.clone(),
                value: choice.value.clone(),
            },
            label: choice.label.clone(),
        });
    }
    entries
}

pub(super) struct ModelPicker {
    workspace: WeakEntity<WorkspaceView>,
    pane: Uuid,
    query: Entity<InputState>,
    active: usize,
    scroll: UniformListScrollHandle,
    pub popover: Option<WeakEntity<PopoverState>>,
    _subscription: Subscription,
    _workspace_subscription: Subscription,
}

impl ModelPicker {
    pub fn new(
        workspace: WeakEntity<WorkspaceView>,
        pane: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::text("搜索模型或提供方…"))
        });
        let _workspace_subscription = cx.observe(
            &workspace.upgrade().expect("workspace owns picker"),
            |_, _, cx| cx.notify(),
        );
        let subscription = cx.subscribe_in(
            &query,
            window,
            |view, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    view.active = 0;
                    view.scroll.scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let selected = view
                        .matches(cx)
                        .into_iter()
                        .filter(|entry| matches!(entry, Entry::Choice { .. }))
                        .nth(view.active);
                    if let Some(Entry::Choice { target, .. }) = selected {
                        view.choose(target, window, cx);
                    }
                }
                _ => {}
            },
        );
        Self {
            workspace,
            pane,
            query,
            active: 0,
            scroll: UniformListScrollHandle::new(),
            popover: None,
            _subscription: subscription,
            _workspace_subscription,
        }
    }

    pub fn focus_handle(&self, cx: &App) -> gpui_kit::FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = 0;
        self.query
            .update(cx, |query, cx| query.set_value("", window, cx));
        self.scroll.scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
        cx.notify();
    }

    fn matches(&self, cx: &App) -> Vec<Entry> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let view = workspace.read(cx);
        let profile = &view.store.profile;
        let mut matches = Vec::new();
        if let Some(agent) = view.selected_agent(self.pane, cx)
            && let Some(pane) = view.panes.get(&self.pane)
            && pane.assistant.agent_settings.session == pane.assistant.session.id
            && pane.assistant.agent_settings.agent == Some(agent.id)
        {
            matches.extend(agent_entries(
                &agent,
                &pane.assistant.agent_settings.configuration,
                self.query.read(cx).value().as_ref(),
            ));
        }
        matches.extend(entries(
            &profile.ai_providers,
            &profile.agents,
            self.query.read(cx).value().as_ref(),
        ));
        matches
    }

    fn dismiss(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(popover) = &self.popover {
            let _ = popover.update(cx, |popover, cx| popover.dismiss(window, cx));
        }
    }

    fn choose(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        let selecting_agent = matches!(target, Target::Agent(_));
        let _ = self.workspace.update(cx, |view, cx| {
            view.select_assistant_target(self.pane, target, window, cx)
        });
        if selecting_agent {
            self.reset(window, cx);
        } else {
            self.dismiss(window, cx);
        }
    }

    fn agent_setting_row(
        &self,
        setting: &crate::acp::configuration::Setting,
        locked: bool,
    ) -> gpui_kit::AnyElement {
        let workspace = self.workspace.clone();
        let pane = self.pane;
        let id = setting.id.clone();
        let current = setting.current.clone();
        let control = if let Some(current) = current.as_bool() {
            crate::controls::button(format!("agent-setting-{id}"))
                .ghost()
                .compact()
                .selected(current)
                .disabled(locked)
                .label(crate::i18n::text(if current { "开启" } else { "关闭" }))
                .on_click(move |_, _, cx| {
                    let _ = workspace.update(cx, |view, cx| {
                        view.configure_agent(pane, id.clone(), (!current).into(), cx)
                    });
                })
                .into_any_element()
        } else {
            let choices: Vec<_> = setting
                .choices
                .iter()
                .map(|choice| {
                    let mut choice = choice.clone();
                    choice.label = agent_option_label(&setting.category, &choice.label);
                    choice
                })
                .collect();
            crate::controls::button(format!("agent-setting-{id}"))
                .ghost()
                .compact()
                .max_w(px(180.))
                .disabled(locked || choices.is_empty())
                .label(agent_option_label(
                    &setting.category,
                    &setting.current_label(),
                ))
                .dropdown_caret(true)
                .dropdown_menu(move |mut menu, _, _| {
                    menu = menu.check_side(gpui_kit::component::Side::Right);
                    for choice in &choices {
                        let workspace = workspace.clone();
                        let id = id.clone();
                        let value = choice.value.clone();
                        menu = menu.item(
                            PopupMenuItem::new(choice.label.clone())
                                .checked(current.as_str() == Some(&choice.value))
                                .on_click(move |_, _, cx| {
                                    let _ = workspace.update(cx, |view, cx| {
                                        view.configure_agent(
                                            pane,
                                            id.clone(),
                                            value.clone().into(),
                                            cx,
                                        )
                                    });
                                }),
                        );
                    }
                    menu
                })
                .into_any_element()
        };
        let label = match setting.label.as_str() {
            "Thinking" => crate::i18n::text("思考模式").to_string(),
            "Effort" | "Reasoning Effort" => crate::i18n::text("推理强度").to_string(),
            "Context" => crate::i18n::text("上下文窗口").to_string(),
            "Mode" => crate::i18n::text("模式").to_string(),
            _ => crate::i18n::localize(setting.label.clone()),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .h(px(34.))
            .min_w_0()
            .child(div().min_w_0().text_ellipsis().child(label))
            .child(control)
            .into_any_element()
    }
}

impl Render for ModelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(workspace) = self.workspace.upgrade() else {
            return div();
        };
        let view = workspace.read(cx);
        let palette = view.palette();
        let selected = view.assistant_target(self.pane, cx);
        let agent = view.selected_agent(self.pane, cx);
        let agent_state = view
            .panes
            .get(&self.pane)
            .filter(|pane| {
                pane.assistant.agent_settings.session == pane.assistant.session.id
                    && pane.assistant.agent_settings.agent == agent.as_ref().map(|agent| agent.id)
            })
            .map(|pane| &pane.assistant.agent_settings);
        let current_agent_model = agent_state
            .and_then(|state| state.configuration.model())
            .and_then(|model| {
                Some(Target::AgentValue {
                    agent: agent.as_ref()?.id,
                    config: model.id.clone(),
                    value: model.current.as_str()?.to_string(),
                })
            });
        let selection = view.pane_selection(self.pane, cx).ok();
        let levels = selection
            .as_ref()
            .and_then(|selection| {
                selection
                    .provider
                    .models
                    .iter()
                    .find(|model| model.id == selection.model)
            })
            .map(|model| Reasoning::available(&model.reasoning))
            .unwrap_or_default();
        let current = selection
            .as_ref()
            .map(|selection| selection.reasoning)
            .unwrap_or_default();
        let locked = view.assistant_locked(self.pane, cx);
        let matches = self.matches(cx);
        let choices = matches
            .iter()
            .filter(|entry| matches!(entry, Entry::Choice { .. }))
            .count();
        self.active = self.active.min(choices.saturating_sub(1));
        let indices: Vec<_> = matches
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| matches!(entry, Entry::Choice { .. }).then_some(i))
            .collect();
        let highlighted = indices.get(self.active).copied();
        let height = (matches.len() as f32 * 34.)
            .clamp(68., 272.)
            .min(f32::from(window.viewport_size().height) * 0.45);
        let weak_workspace = self.workspace.clone();
        let pane = self.pane;
        let reasoning = crate::controls::button("picker-reasoning")
            .ghost()
            .compact()
            .disabled(locked || levels.is_empty())
            .label(
                if selected
                    .as_ref()
                    .is_some_and(|target| matches!(target, Target::Agent(_)))
                {
                    crate::i18n::text("由 Agent 管理")
                } else {
                    current.display_label()
                },
            )
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for level in &levels {
                    let workspace = weak_workspace.clone();
                    let level = *level;
                    menu = menu.item(
                        PopupMenuItem::new(level.display_label())
                            .checked(current == level)
                            .on_click(move |_, window, cx| {
                                let _ = workspace.update(cx, |view, cx| {
                                    view.select_assistant_reasoning(pane, level, window, cx)
                                });
                            }),
                    );
                }
                menu
            });
        let weak_workspace = self.workspace.clone();
        div()
            .flex()
            .flex_col()
            .w(px(344.))
            .max_w(window.viewport_size().width - px(24.))
            .max_h(window.viewport_size().height - px(24.))
            .min_h_0()
            .gap_2()
            .p_2()
            .text_color(rgb(palette.text))
            .bg(rgb(palette.sidebar))
            .capture_key_down(
                cx.listener(move |view, event: &gpui_kit::KeyDownEvent, _, cx| {
                    let key = event.keystroke.key.as_str();
                    if choices > 0
                        && matches!(key, "up" | "down")
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                        && !event.keystroke.modifiers.platform
                    {
                        if key == "down" {
                            view.active = (view.active + 1) % choices;
                        } else {
                            view.active = (view.active + choices - 1) % choices;
                        }
                        view.scroll
                            .scroll_to_item(indices[view.active], gpui_kit::ScrollStrategy::Center);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .child(
                crate::controls::input(&self.query, cx)
                    .w_full()
                    .prefix(Icon::new(IconName::Search).size_4()),
            )
            .children(agent.as_ref().map(|_| {
                let loading = agent_state.is_none_or(|state| state.loading);
                let label = if loading {
                    crate::i18n::text("正在读取 Agent 模型…").to_string()
                } else if let Some(error) = agent_state.and_then(|state| state.error.as_ref()) {
                    crate::i18n::localize(error.clone())
                } else if agent_state.is_some_and(|state| state.configuration.settings.is_empty()) {
                    crate::i18n::text("Agent 未公开设置，使用 CLI 当前配置").to_string()
                } else {
                    crate::i18n::text("模型与设置来自当前 Agent").to_string()
                };
                let workspace = self.workspace.clone();
                let pane = self.pane;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(div().flex_1().min_w_0().text_ellipsis().child(label))
                    .child(
                        crate::controls::button("refresh-agent-models")
                            .ghost()
                            .compact()
                            .disabled(locked || loading)
                            .loading(loading)
                            .icon(Icon::new(IconName::RefreshCw).size_4())
                            .accessibility_label(crate::i18n::text("刷新 Agent 模型"))
                            .on_click(move |_, _, cx| {
                                let _ = workspace.update(cx, |view, cx| {
                                    view.load_agent_configuration(pane, true, cx)
                                });
                            }),
                    )
            }))
            .child(if choices == 0 {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(80.))
                    .text_color(rgb(palette.muted))
                    .child(crate::i18n::text(
                        if agent.is_some() && agent_state.is_none_or(|state| state.loading) {
                            "正在读取 Agent 模型…"
                        } else {
                            "没有匹配的模型"
                        },
                    ))
                    .into_any_element()
            } else {
                uniform_list(
                    "picker-models",
                    matches.len(),
                    cx.processor(
                        move |_: &mut Self,
                              range: std::ops::Range<usize>,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            range
                                .map(|index| match &matches[index] {
                                    Entry::Heading(name) => div()
                                        .h(px(34.))
                                        .flex()
                                        .items_center()
                                        .px_2()
                                        .text_size(px(12.))
                                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                                        .text_color(rgb(palette.muted))
                                        .child(name.clone())
                                        .into_any_element(),
                                    Entry::Choice { target, label } => {
                                        let target = target.clone();
                                        let checked = selected.as_ref() == Some(&target)
                                            || current_agent_model.as_ref() == Some(&target);
                                        crate::controls::button(format!("model-choice-{index}"))
                                            .ghost()
                                            .w_full()
                                            .h(px(34.))
                                            .px_2()
                                            .disabled(locked)
                                            .when(highlighted == Some(index), |button| {
                                                button.bg(rgb(palette.hover))
                                            })
                                            .accessibility_label(label.clone())
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .w_full()
                                                    .min_w_0()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .text_ellipsis()
                                                            .child(label.clone()),
                                                    )
                                                    .children(checked.then(|| {
                                                        Icon::new(IconName::Check)
                                                            .size_4()
                                                            .text_color(rgb(palette.accent))
                                                    })),
                                            )
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.choose(target.clone(), window, cx)
                                            }))
                                            .into_any_element()
                                    }
                                })
                                .collect::<Vec<_>>()
                        },
                    ),
                )
                .h(px(height))
                .track_scroll(&self.scroll)
                .into_any_element()
            })
            .children(agent.is_none().then(|| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(palette.border))
                    .pt_2()
                    .px_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Brain)
                                    .size_4()
                                    .text_color(rgb(palette.muted)),
                            )
                            .child(crate::i18n::text("推理强度")),
                    )
                    .child(reasoning)
            }))
            .children(agent_state.map(|state| {
                div()
                    .id("agent-runtime-settings")
                    .max_h(px(150.))
                    .overflow_y_scroll()
                    .children(
                        state
                            .configuration
                            .settings
                            .iter()
                            .filter(|setting| {
                                setting.category != crate::acp::configuration::Category::Model
                            })
                            .map(|setting| {
                                self.agent_setting_row(setting, locked || state.loading)
                            }),
                    )
            }))
            .child(
                crate::controls::button("picker-configure")
                    .accessibility_label(crate::i18n::text("配置 AI 提供方"))
                    .ghost()
                    .w_full()
                    .h(px(32.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            .gap_2()
                            .text_color(rgb(palette.muted))
                            .child(Icon::new(IconName::Settings2).size_4())
                            .child(crate::i18n::text("配置 AI 提供方")),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.dismiss(window, cx);
                        let _ = weak_workspace.update(cx, |view, cx| {
                            view.settings_form(window, cx);
                            view.settings_page = SettingsPage::Providers;
                            cx.notify();
                        });
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_model_search_preserves_advertised_values_and_agent_identity() {
        let agent = Agent {
            id: Uuid::new_v4(),
            name: "Cursor".into(),
            enabled: true,
            process: crate::integrations::Process {
                program: "cursor-agent".into(),
                args: vec!["acp".into()],
                directory: "/tmp".into(),
                environment: false,
            },
        };
        let configuration = crate::acp::configuration::Configuration::from_response(
            &serde_json::json!({"configOptions":[{
                "id":"model-picker","name":"Model","category":"model","currentValue":"auto-smart","options":[
                    {"group":"cursor","name":"Cursor Models","options":[{"value":"auto-smart","name":"Auto"},{"value":"grok-4.7","name":"Grok 4.7"}]},
                    {"group":"other","name":"Other Models","options":[{"value":"claude","name":"Claude"}]}
                ]
            }]}),
        );
        let found = agent_entries(&agent, &configuration, "CURSOR grok 4.7");
        assert!(
            matches!(found.as_slice(), [Entry::Heading(group), Entry::Choice { target: Target::AgentValue { agent: id, config, value }, label }] if group == "Cursor Models" && *id == agent.id && config == "model-picker" && value == "grok-4.7" && label == "Grok 4.7")
        );
        let auto = agent_entries(&agent, &configuration, "AUTO");
        assert!(
            matches!(auto.as_slice(), [_, Entry::Choice { target: Target::AgentValue { value, .. }, .. }] if value == "auto-smart"),
            "display label must never replace the server's model ID"
        );
        assert!(agent_entries(&agent, &configuration, "absent-model").is_empty());
        let other = Agent {
            id: Uuid::new_v4(),
            ..agent.clone()
        };
        let same_name = agent_entries(&other, &configuration, "auto");
        assert!(
            matches!(same_name.as_slice(), [_, Entry::Choice { target: Target::AgentValue { agent: id, .. }, .. }] if *id == other.id)
        );
    }

    #[test]
    fn search_groups_models_by_provider_preserves_identity_and_omits_empty_groups() {
        let mut first = Provider::legacy(&crate::config::Preferences::default());
        first.name = "Primary".into();
        first.models = vec![crate::provider::Model {
            id: "shared-model".into(),
            reasoning: vec![],
        }];
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.name = "备用".into();
        let mut disabled = first.clone();
        disabled.id = Uuid::new_v4();
        disabled.name = "Disabled".into();
        disabled.enabled = false;
        let providers = [first.clone(), second.clone(), disabled];
        let all = entries(&providers, &[], "SHARED");
        let targets: Vec<_> = all
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Choice { target, .. } => Some(target),
                _ => None,
            })
            .collect();
        assert_eq!(
            targets,
            [
                Target::Model {
                    provider: first.id,
                    model: "shared-model".into()
                },
                Target::Model {
                    provider: second.id,
                    model: "shared-model".into()
                }
            ],
            "identical model IDs from different providers remain distinct"
        );
        let found = entries(&providers, &[], "备用 shared");
        assert!(
            matches!(found.as_slice(), [Entry::Heading(name), Entry::Choice { target: Target::Model { provider, .. }, .. }] if name == "备用" && *provider == second.id)
        );
        assert!(
            entries(&providers, &[], "missing").is_empty(),
            "no orphan provider headings"
        );
        assert!(entries(&providers, &[], "Disabled").is_empty());
    }
}
