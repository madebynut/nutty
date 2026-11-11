//! MCP, ACP and Skills settings with explicit process arguments and protected credentials.

use super::{Form, WorkspaceView, icon_button, modal_container};
use crate::integrations::{Agent, McpServer, McpTransport, Process};
use anyhow::{Context as _, Result, ensure};
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    IndexPath, button::ButtonVariants as _, input::TextareaState, select::SelectState,
    switch::Switch,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, SharedString, Window, div,
    prelude::*, px, rgb,
};
use std::{collections::HashMap, path::PathBuf};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) struct AgentProbe {
    request: Uuid,
    process: Process,
    cancel: CancellationToken,
    result: Option<std::result::Result<usize, String>>,
}
impl Drop for AgentProbe {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IntegrationKind {
    Stdio,
    Http,
    Agent,
    Skills,
}

pub(super) struct IntegrationEditor {
    pub id: Uuid,
    pub saving: bool,
    kind: IntegrationKind,
    pub(super) form: Form,
    data: Entity<TextareaState>,
    enabled: bool,
    credential_exists: bool,
    proxy: Entity<SelectState<crate::controls::Choices>>,
    proxy_ids: Vec<Option<Uuid>>,
}

impl WorkspaceView {
    fn test_agent(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(agent) = self
            .store
            .profile
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .cloned()
        else {
            return;
        };
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.agent_probes.insert(
            id,
            AgentProbe {
                request,
                process: agent.process.clone(),
                cancel: cancel.clone(),
                result: None,
            },
        );
        let events = self.events.clone();
        self.launch(async move {
            let result = crate::acp::probe(agent, cancel).await;
            let _ = events
                .send(super::BackendEvent::AgentProbe {
                    agent: id,
                    request,
                    result,
                })
                .await;
        });
        cx.notify();
    }
    pub(super) fn receive_agent_probe(&mut self, id: Uuid, request: Uuid, result: Result<usize>) {
        if let Some(probe) = self.agent_probes.get_mut(&id)
            && probe.request == request
            && !probe.cancel.is_cancelled()
            && self
                .store
                .profile
                .agents
                .iter()
                .any(|agent| agent.id == id && agent.process == probe.process)
        {
            probe.result = Some(result.map_err(|error| format!("{error:#}")));
        }
    }
    fn agent_actions(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let busy = self
            .agent_probes
            .get(&id)
            .is_some_and(|probe| probe.result.is_none());
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .child(
                crate::controls::button(format!("test-agent-{id}"))
                    .compact()
                    .loading(busy)
                    .disabled(busy)
                    .label(crate::i18n::text(if busy {
                        "测试中…"
                    } else {
                        "测试可用性"
                    }))
                    .on_click(cx.listener(move |view, _, _, cx| view.test_agent(id, cx))),
            )
            .child(
                icon_button(
                    format!("edit-agent-{id}"),
                    IconName::Settings,
                    crate::i18n::text("设置"),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.edit_integration(IntegrationKind::Agent, Some(id), window, cx)
                })),
            )
            .child(
                crate::controls::button(format!("remove-agent-{id}"))
                    .ghost()
                    .compact()
                    .icon(gpui_kit::component::Icon::new(IconName::Trash).size_4())
                    .label(crate::i18n::text("移除"))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.remove_integration(IntegrationKind::Agent, id, cx)
                    })),
            )
            .into_any_element()
    }
    fn probe_status(&self, agent: &Agent) -> Option<(bool, String)> {
        let probe = self
            .agent_probes
            .get(&agent.id)
            .filter(|p| p.process == agent.process)?;
        probe.result.as_ref().map(|result| match result {
            Ok(count) => (
                true,
                crate::i18n::localize(format!("认证与会话创建通过 · {} 个模型", count)),
            ),
            Err(error) => (
                false,
                format!(
                    "{} · {}",
                    crate::i18n::text("测试失败"),
                    crate::i18n::localize(error.clone())
                ),
            ),
        })
    }
    fn import_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.skills_import.is_some() {
            return;
        }
        let request = Uuid::new_v4();
        self.skills_import = Some(request);
        let prompt = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some(crate::i18n::text("选择 SKILL.md、技能文件夹或技能目录").into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let selection = match prompt.await {
                Ok(Ok(Some(paths))) => Ok(paths),
                Ok(Err(error)) => Err(error),
                _ => Ok(Vec::new()),
            };
            let _ = view.update(cx, |view, cx| {
                if matches!(&selection, Ok(paths) if paths.is_empty()) {
                    view.skills_import = None;
                    cx.notify();
                    return;
                }
                let roots = view.store.profile.skills_roots.clone();
                let base = roots.clone();
                let events = view.events.clone();
                view.launch(async move {
                    let result = match selection {
                        Ok(paths) => tokio::task::spawn_blocking(move || {
                            crate::integrations::import_skill_roots(&roots, paths)
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.into())),
                        Err(error) => Err(error),
                    };
                    let _ = events
                        .send(super::BackendEvent::SkillsImported {
                            request,
                            base,
                            result,
                        })
                        .await;
                });
                cx.notify();
            });
        })
        .detach();
    }
    fn browse_skill_directories(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .integration_editor
            .as_ref()
            .filter(|editor| editor.kind == IntegrationKind::Skills && !editor.saving)
        else {
            return;
        };
        let id = editor.id;
        let input = editor.data.downgrade();
        let prompt = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some(crate::i18n::text("选择文件夹").into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let selection = prompt.await;
            let _ = view.update_in(cx, |view, window, cx| {
                let Some(editor) = view.integration_editor.as_mut().filter(|editor| {
                    editor.id == id
                        && editor.kind == IntegrationKind::Skills
                        && !editor.saving
                        && editor.data.downgrade() == input
                }) else {
                    return;
                };
                match selection {
                    Ok(Ok(Some(paths))) => {
                        let mut roots: Vec<_> = editor
                            .data
                            .read(cx)
                            .value()
                            .lines()
                            .map(str::to_owned)
                            .collect();
                        for path in paths {
                            let path = path.to_string_lossy().into_owned();
                            if !roots.contains(&path) {
                                roots.push(path);
                            }
                        }
                        editor
                            .data
                            .update(cx, |data, cx| data.set_value(roots.join("\n"), window, cx));
                    }
                    Ok(Err(error)) => view.status = Some(error.to_string()),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn receive_skills_import(
        &mut self,
        request: Uuid,
        base: Vec<PathBuf>,
        result: Result<Vec<PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        if self.skills_import != Some(request) {
            return;
        }
        match result {
            Ok(roots) => {
                if self.store.profile.skills_roots != base
                    || self.configuration_save.is_some()
                    || self.data_busy.is_some()
                {
                    let selected = roots
                        .into_iter()
                        .filter(|root| !base.contains(root))
                        .collect();
                    let base = self.store.profile.skills_roots.clone();
                    let roots = base.clone();
                    let events = self.events.clone();
                    self.launch(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        let result = tokio::task::spawn_blocking(move || {
                            crate::integrations::import_skill_roots(&roots, selected)
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.into()));
                        let _ = events
                            .send(super::BackendEvent::SkillsImported {
                                request,
                                base,
                                result,
                            })
                            .await;
                    });
                    return;
                }
                self.skills_import = None;
                if roots == base {
                    return;
                }
                let mut profile = self.store.profile.clone();
                profile.skills_roots = roots;
                if let Err(error) = self.persist_profile(request, profile, Vec::new(), cx) {
                    self.status = Some(format!("{error:#}"));
                }
            }
            Err(error) => {
                self.skills_import = None;
                self.status = Some(format!("{error:#}"));
            }
        }
    }
    pub(super) fn scan_local_agents(&mut self, cx: &mut Context<Self>) {
        if self.agent_scan.is_some() {
            return;
        }
        let request = Uuid::new_v4();
        self.agent_scan = Some(request);
        let events = self.events.clone();
        self.launch(async move {
            let agents = crate::agent_discovery::installed().await;
            let _ = events
                .send(super::BackendEvent::LocalAgents { request, agents })
                .await;
        });
        cx.notify();
    }

    fn use_local_agent(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detected) = self.discovered_agents.get(index).cloned() else {
            return;
        };
        let Some(launch) = detected.launch else {
            return;
        };
        let existing = self
            .store
            .profile
            .agents
            .iter()
            .find(|agent| {
                agent.process.program == launch.program && agent.process.args == launch.args
            })
            .map(|agent| agent.id);
        self.edit_integration(IntegrationKind::Agent, existing, window, cx);
        if existing.is_some() {
            return;
        }
        if let Some(editor) = &mut self.integration_editor {
            for (field, value) in [
                ("name", detected.name.to_string()),
                ("program", launch.program.display().to_string()),
                (
                    "directory",
                    self.store
                        .profile
                        .preferences
                        .directory
                        .display()
                        .to_string(),
                ),
            ] {
                if let Some(input) = editor.form.fields.get(field) {
                    input.update(cx, |input, cx| input.set_value(value, window, cx));
                }
            }
            let args = serde_json::to_string_pretty(&launch.args).unwrap_or_default();
            editor
                .data
                .update(cx, |input, cx| input.set_value(args, window, cx));
        }
        cx.notify();
    }

    fn render_local_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(crate::i18n::text("本地 Agent"))
                    .child(
                        crate::controls::button("refresh-local-agents")
                            .compact()
                            .loading(self.agent_scan.is_some())
                            .disabled(self.agent_scan.is_some())
                            .label(crate::i18n::text("重新识别"))
                            .on_click(cx.listener(|view, _, _, cx| view.scan_local_agents(cx))),
                    ),
            )
            .children(
                self.discovered_agents
                    .iter()
                    .enumerate()
                    .map(|(index, detected)| {
                        let configured = detected.launch.as_ref().and_then(|launch| {
                            self.store.profile.agents.iter().find(|agent| {
                                agent.process.program == launch.program
                                    && agent.process.args == launch.args
                            })
                        });
                        let detail = match &detected.launch {
                            Some(launch) if launch.adapter => {
                                "官方 ACP 适配器 · 首次启动会自动下载"
                            }
                            Some(_) => "ACP 启动配置已识别",
                            None => "已安装 · 需安装 Node.js 或 ACP 适配器",
                        };
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .bg(rgb(p.surface))
                            .when_some(configured, |row, agent| {
                                row.group(gpui_kit::SharedString::from(format!(
                                    "agent-actions-{}",
                                    agent.id
                                )))
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(
                                        gpui_kit::component::Icon::new(IconName::Sparkles).size_5(),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_1()
                                            .child(div().text_ellipsis().child(format!(
                                                "{} {}",
                                                detected.name, detected.version
                                            )))
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(rgb(p.muted))
                                                    .child(crate::i18n::text(detail)),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(rgb(p.muted))
                                                    .text_ellipsis()
                                                    .child(
                                                        detected.executable.display().to_string(),
                                                    ),
                                            ),
                                    )
                                    .children(
                                        configured.map(|agent| self.agent_actions(agent.id, cx)),
                                    )
                                    .children(configured.is_none().then(|| {
                                        crate::controls::button(format!("use-local-agent-{index}"))
                                            .compact()
                                            .label(crate::i18n::text("接入"))
                                            .disabled(
                                                detected.launch.is_none()
                                                    || self.configuration_save.is_some(),
                                            )
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.use_local_agent(index, window, cx)
                                            }))
                                    })),
                            )
                            .children(configured.and_then(|agent| self.probe_status(agent)).map(
                                |(success, status)| {
                                    div()
                                        .text_size(px(12.))
                                        .text_color(rgb(if success {
                                            p.success
                                        } else {
                                            p.ansi[1]
                                        }))
                                        .child(status)
                                },
                            ))
                    }),
            )
            .children(
                (self.discovered_agents.is_empty() && self.agent_scan.is_none()).then(|| {
                    div().text_size(px(12.)).child(crate::i18n::text(
                        "未识别到本地 Agent，仍可手动添加 ACP 程序",
                    ))
                }),
            )
            .into_any_element()
    }
    pub(super) fn render_integrations_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        let add = |id, kind, label: &str, cx: &mut Context<Self>| {
            crate::controls::button(id)
                .compact()
                .label(crate::i18n::text(label).to_owned())
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.edit_integration(kind, None, window, cx)
                }))
        };
        let mcp = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .flex_wrap()
                    .child(add(
                        "add-mcp-stdio",
                        IntegrationKind::Stdio,
                        "添加 MCP 程序",
                        cx,
                    ))
                    .child(add(
                        "add-mcp-http",
                        IntegrationKind::Http,
                        "添加 MCP HTTP",
                        cx,
                    )),
            )
            .children(self.store.profile.mcp_servers.iter().map(|server| {
                self.integration_row(
                    server.id,
                    match server.transport {
                        McpTransport::Stdio { .. } => IntegrationKind::Stdio,
                        McpTransport::Http { .. } => IntegrationKind::Http,
                    },
                    &server.name,
                    if server.enabled {
                        crate::i18n::text("MCP · 已启用")
                    } else {
                        crate::i18n::text("MCP · 已禁用")
                    },
                    cx,
                )
            }))
            .children(self.store.profile.mcp_servers.is_empty().then(|| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text("尚未配置 MCP 服务"))
            }))
            .into_any_element();
        let agents = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_local_agents(cx))
            .child(add(
                "add-acp-agent",
                IntegrationKind::Agent,
                "添加 ACP Agent",
                cx,
            ))
            .children(
                self.store
                    .profile
                    .agents
                    .iter()
                    .filter(|agent| {
                        !self.discovered_agents.iter().any(|detected| {
                            detected.launch.as_ref().is_some_and(|launch| {
                                agent.process.program == launch.program
                                    && agent.process.args == launch.args
                            })
                        })
                    })
                    .map(|agent| {
                        self.integration_row(
                            agent.id,
                            IntegrationKind::Agent,
                            &agent.name,
                            if agent.enabled {
                                crate::i18n::text("ACP · 已启用")
                            } else {
                                crate::i18n::text("ACP · 已禁用")
                            },
                            cx,
                        )
                    }),
            )
            .children(self.store.profile.agents.is_empty().then(|| {
                div()
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text("尚未配置 Agent"))
            }))
            .into_any_element();
        let skills =
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    crate::controls::button("import-skills")
                        .compact()
                        .icon(IconName::FolderOpen)
                        .label(crate::i18n::text("从文件系统导入 Skills"))
                        .loading(self.skills_import.is_some())
                        .disabled(self.skills_import.is_some() || self.configuration_save.is_some())
                        .on_click(
                            cx.listener(|view, _, window, cx| view.import_skills(window, cx)),
                        ),
                )
                .child(add(
                    "edit-skills-roots",
                    IntegrationKind::Skills,
                    "高级：手动配置路径",
                    cx,
                ))
                .children(self.store.profile.skills_roots.iter().enumerate().map(
                    |(index, path)| {
                        let path = path.clone();
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .text_size(px(12.))
                                    .text_color(rgb(p.muted))
                                    .child(path.display().to_string()),
                            )
                            .child(
                                icon_button(
                                    format!("remove-skill-root-{index}"),
                                    IconName::X,
                                    crate::i18n::text("移除"),
                                )
                                .disabled(
                                    self.configuration_save.is_some()
                                        || self.skills_import.is_some(),
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        let mut profile = view.store.profile.clone();
                                        profile.skills_roots.retain(|root| *root != path);
                                        if let Err(error) = view.persist_profile(
                                            Uuid::new_v4(),
                                            profile,
                                            Vec::new(),
                                            cx,
                                        ) {
                                            view.status = Some(format!("{error:#}"));
                                        }
                                    },
                                )),
                            )
                    },
                ))
                .children(self.store.profile.skills_roots.is_empty().then(|| {
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text("未配置"))
                }))
                .into_any_element();
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.settings_heading(
                "工具与扩展",
                "工具调用显示参数供你确认。连接、工具结果和代理会话随所属终端结束或重置。",
            ))
            .child(self.settings_group("MCP 服务", "为 AI 会话连接外部工具", mcp))
            .child(self.settings_group("ACP Agents", "使用独立的 Agent 程序处理终端任务", agents))
            .child(self.settings_group("Skills", "从指定目录加载技能说明", skills))
            .into_any_element()
    }
    fn integration_row(
        &self,
        id: Uuid,
        kind: IntegrationKind,
        name: &str,
        detail: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette();
        div()
            .flex()
            .items_center()
            .justify_between()
            .p_3()
            .rounded_md()
            .bg(rgb(p.surface))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(div().text_ellipsis().child(name.to_string()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(detail.to_string()),
                    )
                    .children(
                        self.store
                            .profile
                            .agents
                            .iter()
                            .find(|a| kind == IntegrationKind::Agent && a.id == id)
                            .and_then(|agent| self.probe_status(agent))
                            .map(|(success, status)| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(if success { p.success } else { p.ansi[1] }))
                                    .child(status)
                            }),
                    ),
            )
            .children((kind == IntegrationKind::Agent).then(|| self.agent_actions(id, cx)))
            .children((kind != IntegrationKind::Agent).then(|| {
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        icon_button(
                            format!("edit-integration-{id}"),
                            IconName::Settings,
                            crate::i18n::text("编辑"),
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.edit_integration(kind, Some(id), window, cx)
                            },
                        )),
                    )
                    .child(
                        icon_button(
                            format!("remove-integration-{id}"),
                            IconName::X,
                            crate::i18n::text("移除"),
                        )
                        .on_click(
                            cx.listener(move |view, _, _, cx| {
                                view.remove_integration(kind, id, cx)
                            }),
                        ),
                    )
            }))
            .into_any_element()
    }
    fn edit_integration(
        &mut self,
        kind: IntegrationKind,
        id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        if kind == IntegrationKind::Agent
            && let Some(id) = id
        {
            self.agent_probes.remove(&id);
        }
        let server = id.and_then(|id| {
            self.store
                .profile
                .mcp_servers
                .iter()
                .find(|server| server.id == id)
        });
        let agent = id.and_then(|id| {
            self.store
                .profile
                .agents
                .iter()
                .find(|agent| agent.id == id)
        });
        let process = agent.map(|agent| &agent.process).or_else(|| {
            server.and_then(|server| match &server.transport {
                McpTransport::Stdio { process } => Some(process),
                _ => None,
            })
        });
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let directory = process
            .map(|process| process.directory.as_path())
            .unwrap_or(&home)
            .display()
            .to_string();
        let program = process
            .map(|process| process.program.display().to_string())
            .unwrap_or_default();
        let (url, bearer, proxy) = match server.map(|server| &server.transport) {
            Some(McpTransport::Http { url, bearer, proxy }) => (url.as_str(), *bearer, *proxy),
            _ => ("https://example.com/mcp", false, None),
        };
        let mut proxy_ids = vec![None];
        let mut names = vec![SharedString::from(crate::i18n::text("直连"))];
        for item in &self.store.profile.proxies {
            proxy_ids.push(Some(item.id));
            names.push(item.name.clone().into());
        }
        let selected = proxy_ids.iter().position(|id| *id == proxy).unwrap_or(0);
        let proxy = cx.new(|cx| {
            crate::controls::select_state(names, Some(IndexPath::new(selected)), window, cx)
        });
        let form = Form::new(
            &[
                (
                    "name",
                    server
                        .map(|server| server.name.as_str())
                        .or_else(|| agent.map(|agent| agent.name.as_str()))
                        .unwrap_or(""),
                ),
                ("program", &program),
                ("directory", &directory),
                ("url", url),
                ("key_secret", ""),
            ],
            window,
            cx,
        );
        let initial = if kind == IntegrationKind::Skills {
            self.store
                .profile
                .skills_roots
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            serde_json::to_string_pretty(
                &process
                    .map(|process| process.args.as_slice())
                    .unwrap_or_default(),
            )
            .unwrap_or_default()
        };
        let data = cx.new(|cx| TextareaState::new(window, cx).default_value(initial));
        self.integration_editor = Some(IntegrationEditor {
            id: id.unwrap_or_else(Uuid::new_v4),
            saving: false,
            kind,
            form,
            data,
            enabled: server.is_none_or(|server| server.enabled)
                && agent.is_none_or(|agent| agent.enabled),
            credential_exists: bearer || process.is_some_and(|process| process.environment),
            proxy,
            proxy_ids,
        });
        cx.notify();
    }
    pub(super) fn render_integration_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self
            .integration_editor
            .as_ref()
            .expect("integration editor is open");
        let p = self.palette();
        let title = match editor.kind {
            IntegrationKind::Stdio => crate::i18n::text("MCP 程序"),
            IntegrationKind::Http => "MCP HTTP",
            IntegrationKind::Agent => "ACP Agent",
            IntegrationKind::Skills => crate::i18n::text("Skills 目录"),
        };
        let mut content = div()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .w(px(680.))
            .max_h(px(760.))
            .rounded_lg()
            .bg(rgb(p.sidebar))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(18.)).child(title))
                    .child(
                        icon_button(
                            "close-integration",
                            IconName::X,
                            crate::i18n::text("返回设置"),
                        )
                        .disabled(editor.saving)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.integration_editor = None;
                            cx.notify();
                        })),
                    ),
            );
        if editor.kind == IntegrationKind::Skills {
            content = content
                .child(
                    crate::controls::button("browse-skill-directories")
                        .ghost()
                        .compact()
                        .icon(gpui_kit::component::Icon::new(IconName::FolderOpen).size_4())
                        .label(crate::i18n::text("浏览…"))
                        .disabled(editor.saving)
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.browse_skill_directories(window, cx)
                        })),
                )
                .child(crate::i18n::text(
                    "每行一个目录；目录内各 Skill 使用独立文件夹和 SKILL.md。",
                ))
                .child(
                    crate::controls::textarea(&editor.data, cx)
                        .w_full()
                        .h(px(220.)),
                );
        } else {
            content = content.child(editor.form.field("name", crate::i18n::text("名称"), cx));
            if editor.kind == IntegrationKind::Http {
                content = content
                    .child(
                        editor
                            .form
                            .field("url", crate::i18n::text("Streamable HTTP 地址"), cx),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(crate::i18n::text("网络代理"))
                            .child(
                                crate::controls::select(&editor.proxy, cx)
                                    .w_full()
                                    .disabled(editor.saving),
                            ),
                    )
                    .child(editor.form.field(
                        "key_secret",
                        crate::i18n::text("Bearer Token · 留空保留"),
                        cx,
                    ));
            } else {
                content = content
                    .child(editor.form.path_field(
                        "program",
                        crate::i18n::text("程序路径或名称"),
                        super::paths::PathKind::File,
                        cx,
                    ))
                    .child(editor.form.path_field(
                        "directory",
                        crate::i18n::text("工作目录"),
                        super::paths::PathKind::Directory,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(crate::i18n::text("程序参数 · JSON 数组"))
                            .child(
                                crate::controls::textarea(&editor.data, cx)
                                    .w_full()
                                    .h(px(100.)),
                            ),
                    )
                    .child(editor.form.field(
                        "key_secret",
                        crate::i18n::text("环境变量 · JSON 对象，保存在钥匙串，留空保留"),
                        cx,
                    ));
            }
            content = content.child(
                Switch::new("integration-enabled")
                    .cursor_pointer()
                    .label(crate::i18n::text("启用"))
                    .checked(editor.enabled)
                    .disabled(editor.saving)
                    .on_click(cx.listener(|view, value: &bool, _, cx| {
                        if let Some(editor) = &mut view.integration_editor {
                            editor.enabled = *value;
                        }
                        cx.notify();
                    })),
            );
        }
        content = content.child(
            crate::controls::button("save-integration")
                .primary()
                .label(if editor.saving {
                    crate::i18n::text("保存中…")
                } else {
                    crate::i18n::text("保存")
                })
                .disabled(editor.saving)
                .on_click(cx.listener(|view, _, _, cx| view.save_integration(cx))),
        );
        modal_container(p).child(content).into_any_element()
    }
    fn save_integration(&mut self, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        let result = (|| -> Result<_> {
            let editor = self
                .integration_editor
                .as_ref()
                .context(crate::i18n::text("配置已关闭"))?;
            let mut profile = self.store.profile.clone();
            let credential = zeroize::Zeroizing::new(editor.form.value("key_secret", cx));
            let mut secret = None;
            if editor.kind == IntegrationKind::Skills {
                profile.skills_roots = editor
                    .data
                    .read(cx)
                    .value()
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .collect();
            } else {
                let process = if editor.kind != IntegrationKind::Http {
                    let args: Vec<String> = serde_json::from_str(&editor.data.read(cx).value())
                        .context(crate::i18n::text("参数必须是 JSON 字符串数组"))?;
                    if !credential.is_empty() {
                        let values: HashMap<String, String> = serde_json::from_str(&credential)
                            .context(crate::i18n::text("环境变量必须是 JSON 字符串对象"))?;
                        crate::integrations::validate_environment(&values)?;
                    }
                    Some(Process {
                        program: PathBuf::from(editor.form.value("program", cx)),
                        args,
                        directory: PathBuf::from(editor.form.value("directory", cx)),
                        environment: !credential.is_empty() || editor.credential_exists,
                    })
                } else {
                    None
                };
                if editor.kind == IntegrationKind::Agent {
                    let agent = Agent {
                        id: editor.id,
                        name: editor.form.value("name", cx),
                        enabled: editor.enabled,
                        process: process.context(crate::i18n::text("程序配置缺失"))?,
                    };
                    if let Some(previous) =
                        profile.agents.iter_mut().find(|item| item.id == agent.id)
                    {
                        *previous = agent;
                    } else {
                        profile.agents.push(agent);
                    }
                } else {
                    let transport = if editor.kind == IntegrationKind::Http {
                        McpTransport::Http {
                            url: editor.form.value("url", cx),
                            bearer: !credential.is_empty() || editor.credential_exists,
                            proxy: editor
                                .proxy
                                .read(cx)
                                .selected_index(cx)
                                .and_then(|index| editor.proxy_ids.get(index.row))
                                .copied()
                                .flatten(),
                        }
                    } else {
                        McpTransport::Stdio {
                            process: process.context(crate::i18n::text("程序配置缺失"))?,
                        }
                    };
                    let server = McpServer {
                        id: editor.id,
                        name: editor.form.value("name", cx),
                        enabled: editor.enabled,
                        transport,
                    };
                    if let Some(previous) = profile
                        .mcp_servers
                        .iter_mut()
                        .find(|item| item.id == server.id)
                    {
                        *previous = server;
                    } else {
                        profile.mcp_servers.push(server);
                    }
                }
                if !credential.is_empty() {
                    ensure!(
                        credential.len() <= 256 * 1024,
                        crate::i18n::text("凭据过长")
                    );
                    secret = Some((format!("integration-{}", editor.id), credential));
                }
            }
            profile.validate()?;
            Ok((editor.id, profile, secret))
        })();
        match result {
            Ok((id, profile, secret)) => {
                if let Some(editor) = &mut self.integration_editor {
                    editor.saving = true;
                }
                if let Err(error) =
                    self.persist_profile(id, profile, secret.into_iter().collect(), cx)
                {
                    if let Some(editor) = &mut self.integration_editor {
                        editor.saving = false;
                    }
                    self.status = Some(error.to_string());
                }
            }
            Err(error) => self.status = Some(format!("{error:#}")),
        };
        cx.notify();
    }
    fn remove_integration(&mut self, kind: IntegrationKind, id: Uuid, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.data_busy.is_some() {
            return;
        }
        let mut profile = self.store.profile.clone();
        match kind {
            IntegrationKind::Agent => {
                self.agent_probes.remove(&id);
                profile.agents.retain(|agent| agent.id != id);
            }
            _ => profile.mcp_servers.retain(|server| server.id != id),
        }
        if let Err(error) = self.persist_profile(id, profile, Vec::new(), cx) {
            self.status = Some(error.to_string());
        }
        cx.notify();
    }
}
