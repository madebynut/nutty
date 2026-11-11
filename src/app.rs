//! Native workspace chrome and user-owned terminal, file, forwarding, and AI workflows.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context as _, Result, ensure};
use async_channel::Sender;
use gpui_kit::assets::IconName;
use gpui_kit::base::Disableable as _;
use gpui_kit::component::{
    Icon, IndexPath,
    button::{Button, ButtonVariants as _},
    input::{InputEvent, InputState, TextareaState},
    menu::DropdownMenu as _,
    resizable::{h_resizable, resizable_panel, v_resizable},
    select::SelectState,
};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, Render, Subscription, Task, Window,
    div, prelude::*, px, rgb, rgba, uniform_list,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    ai,
    assistant_session::Mode,
    config::{Appearance, Authentication, Connection, Store},
    remote::{AuthPrompt, Forward, HostPrompt, Node, Registry, RemoteFile},
    terminal::Terminal,
    terminal_view::{PaneEvent, TerminalView},
    theme::Palette,
    workspace::{Axis, Layout},
};

mod agent_settings;
mod assistant;
mod authentication;
mod chrome;
mod composer;
mod connection_attempts;
mod connections;
mod data;
mod files;
mod forwarding;
mod integrations;
mod model_picker;
mod notifications;
mod panels;
mod paths;
mod session_restore;
mod settings;
mod sidebar;
mod transfer_queue;
use assistant::PaneAssistant;
use chrome::Overlay;
use integrations::IntegrationEditor;
use panels::{PaneDock, PanelBounds, PanelEdge, PanelSize, ResizePanel};
use settings::{RecordEditor, SettingsPage};

struct Pane {
    source: crate::workspace::session::Source,
    engine: crate::terminal::Engine,
    file_target: Option<crate::terminal::DirectoryTarget>,
    file_binding: Option<(Uuid, Arc<Node>)>,
    view: Entity<TerminalView>,
    node: Option<Arc<Node>>,
    assistant: PaneAssistant,
    reconnect: Option<(Uuid, CancellationToken)>,
    directory: Option<String>,
    dock: PaneDock,
    bounds: PanelBounds,
    _observe: Subscription,
    _events: Subscription,
}
impl Pane {
    fn dock(&self) -> &PanelSize {
        self.dock.size()
    }
    fn dock_mut(&mut self) -> &mut PanelSize {
        self.dock.size_mut()
    }
    fn dock_limit(&self) -> f32 {
        let height = f32::from(self.bounds.get().size.height);
        if height > 0. { height * 0.5 } else { 240. }
    }
    fn dock_expanded(&self) -> bool {
        self.dock().extent(self.dock_limit()) > 0.
    }
}

struct Tab {
    id: Uuid,
    name: String,
    layout: Layout,
    active_pane: Uuid,
}

#[derive(Clone, Copy)]
struct Placement {
    tab: Uuid,
    pane: Uuid,
    axis: Axis,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Hidden,
    History,
    Files,
    Forwarding,
}
use nutty_core::forwarding::Mode as ForwardMode;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Modal {
    Connection,
    Settings,
}

struct Form {
    agent_probe: Option<connections::AgentProbe>,
    agent_result: Option<std::result::Result<crate::ssh_agent::Info, String>>,
    agent_keys: Option<Entity<SelectState<crate::controls::Choices>>>,
    agent_key_ids: Vec<Option<String>>,
    id: Uuid,
    algorithms: crate::ssh_options::Algorithms,
    algorithm_category: Option<crate::ssh_options::Category>,
    connection_probe: Option<connections::ConnectionProbe>,
    probe_result: Option<(bool, String)>,
    fields: HashMap<&'static str, Entity<InputState>>,
    editing: Option<Uuid>,
    auth: u8,
    appearance: Option<Appearance>,
    jump: Option<Entity<SelectState<crate::controls::Choices>>>,
    jump_ids: Vec<Option<Uuid>>,
    proxy: Option<Entity<SelectState<crate::controls::Choices>>>,
    proxy_ids: Vec<Option<Uuid>>,
    ssh_reconnect: bool,
    connection_kind: crate::config::ConnectionKind,
    preferences: Option<crate::config::Preferences>,
    language: Option<Entity<SelectState<crate::controls::Choices>>>,
    fonts: Option<settings::FontEditor>,
    shell: Option<settings::ShellEditor>,
    terminal: Option<settings::TerminalEditor>,
    theme: Option<settings::ThemeEditor>,
}
impl Form {
    fn new(
        fields: &[(&'static str, &str)],
        window: &mut Window,
        cx: &mut Context<WorkspaceView>,
    ) -> Self {
        Self {
            agent_probe: None,
            agent_result: None,
            agent_keys: None,
            agent_key_ids: Vec::new(),
            id: Uuid::new_v4(),
            algorithms: Default::default(),
            algorithm_category: None,
            connection_probe: None,
            probe_result: None,
            fields: fields
                .iter()
                .map(|(key, value)| {
                    (
                        *key,
                        cx.new(|cx| {
                            InputState::new(window, cx)
                                .default_value(value.to_string())
                                .masked(matches!(
                                    *key,
                                    "password" | "ai_key" | "key_secret" | "token"
                                ))
                        }),
                    )
                })
                .collect(),
            editing: None,
            auth: 0,
            appearance: None,
            jump: None,
            jump_ids: Vec::new(),
            proxy: None,
            proxy_ids: Vec::new(),
            ssh_reconnect: true,
            connection_kind: Default::default(),
            preferences: None,
            language: None,
            fonts: None,
            shell: None,
            terminal: None,
            theme: None,
        }
    }
    fn value(&self, key: &str, cx: &Context<WorkspaceView>) -> String {
        self.fields
            .get(key)
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default()
    }
    fn field(&self, key: &str, label: &str, cx: &Context<WorkspaceView>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().text_size(px(12.)).child(label.to_string()))
            .children(
                self.fields
                    .get(key)
                    .map(|state| crate::controls::input(state, cx).w_full()),
            )
            .into_any_element()
    }
}

enum BackendEvent {
    SshAgentProbe {
        form: Uuid,
        request: Uuid,
        result: Result<crate::ssh_agent::Info>,
    },
    AgentProbe {
        agent: Uuid,
        request: Uuid,
        result: Result<usize>,
    },
    SkillsImported {
        request: Uuid,
        base: Vec<PathBuf>,
        result: Result<Vec<PathBuf>>,
    },
    AgentConfiguration {
        pane: Uuid,
        identity: agent_settings::Identity,
        request: Option<Uuid>,
        result: Result<crate::acp::configuration::Configuration>,
    },
    ConnectionProbe {
        form: Uuid,
        request: Uuid,
        result: Result<()>,
    },
    LocalAgents {
        request: Uuid,
        agents: Vec<crate::agent_discovery::Detected>,
    },
    ProviderCatalog {
        editor: Uuid,
        request: Uuid,
        apply: bool,
        result: Result<Vec<crate::provider::Model>>,
    },
    DataLoaded {
        id: Uuid,
        result: Result<crate::portable::Preview>,
    },
    DataExported {
        id: Uuid,
        result: Result<()>,
    },
    AiProgress {
        pane: Uuid,
        session: Uuid,
        request: Uuid,
        text: String,
    },
    ShutdownReady,
    ShutdownFailed(String),
    AgentPermission {
        pane: Uuid,
        session: Uuid,
        permission: crate::acp::Permission,
    },
    ToolResult {
        pane: Uuid,
        session: Uuid,
        tool: Uuid,
        result: Result<String>,
    },
    ConfigurationSaved {
        id: Uuid,
        result: Result<crate::config::Profile>,
    },
    Authentication(AuthPrompt),
    Connected {
        id: Uuid,
        request: Uuid,
        split: Option<Placement>,
        result: Result<(Arc<Node>, Terminal)>,
    },
    Reconnected {
        pane: Uuid,
        request: Uuid,
        result: Result<(Option<Arc<Node>>, Terminal)>,
    },
    SftpBound {
        request: Uuid,
        pane: Uuid,
        target: Uuid,
        node: Arc<Node>,
        result: Result<()>,
    },
    SftpConnected {
        request: Uuid,
        result: Result<Arc<Node>>,
    },
    Files {
        request: u64,
        result: Result<Vec<RemoteFile>>,
    },
    Preview {
        request: u64,
        node: Arc<Node>,
        path: String,
        result: Result<String>,
    },
    FileActionDone {
        id: Uuid,
        result: Result<()>,
    },
    FileSaved {
        id: Uuid,
        content: String,
        result: Result<()>,
    },
    Transfers,
    Ai {
        pane: Uuid,
        session: Uuid,
        request: Uuid,
        result: Result<ai::Reply>,
    },
    Forward {
        rule: Uuid,
        request: Uuid,
        result: Result<Forward>,
    },
    Error(String),
}

struct AuthDialog {
    prompt: AuthPrompt,
    fields: Vec<Entity<InputState>>,
    _subscriptions: Vec<gpui_kit::Subscription>,
}

struct FileEditor {
    id: Uuid,
    node: Arc<Node>,
    path: String,
    original: String,
    input: Entity<TextareaState>,
    saving: bool,
}

pub(crate) struct WorkspaceView {
    store: Store,
    workspace_writer: crate::workspace::session::Writer,
    workspace_checkpoint: Option<crate::workspace::session::Snapshot>,
    split_sizes: HashMap<Uuid, [f32; 2]>,
    _workspace_timer: Option<Task<()>>,
    runtime: Arc<tokio::runtime::Runtime>,
    registry: Arc<Registry>,
    events: Sender<BackendEvent>,
    jobs: Vec<tokio::task::JoinHandle<()>>,
    tabs: Vec<Tab>,
    panes: HashMap<Uuid, Pane>,
    active_tab: Option<Uuid>,
    active_pane: Option<Uuid>,
    connecting: connection_attempts::ConnectionAttempts,
    sidebar: PanelSize,
    collapsed_connection_groups: HashSet<Option<String>>,
    tool_size: PanelSize,
    panel_bounds: PanelBounds,
    broadcast: bool,
    tool: Tool,
    modal: Option<Modal>,
    modal_focus: gpui_kit::FocusHandle,
    form: Option<Form>,
    jump_drafts: Vec<connections::JumpDraft>,
    connection_removal: Option<connections::ConnectionRemoval>,
    connection_proxy_editor: Option<(Uuid, Uuid)>,
    overlay: Option<Overlay>,
    palette_input: Entity<InputState>,
    settings_page: SettingsPage,
    discovered_agents: Vec<crate::agent_discovery::Detected>,
    agent_scan: Option<Uuid>,
    agents_scanned: bool,
    agent_probes: HashMap<Uuid, integrations::AgentProbe>,
    skills_import: Option<Uuid>,
    record_editor: Option<RecordEditor>,
    integration_editor: Option<IntegrationEditor>,
    status: Option<String>,
    success_notification: notifications::SuccessNotification,
    _success_notification_timer: Option<Task<()>>,
    configuration_save: Option<Uuid>,
    shutting_down: bool,
    data_busy: Option<Uuid>,
    data_apply: Option<Uuid>,
    data_password: Entity<InputState>,
    data_credentials: bool,
    data_replace: bool,
    data_preview: Option<crate::portable::Preview>,
    filter: Entity<InputState>,
    prompts: VecDeque<HostPrompt>,
    auth_dialog: Option<AuthDialog>,
    pending_authentication: VecDeque<AuthPrompt>,
    file_connection: Option<Arc<Node>>,
    file_connect: Option<(Uuid, CancellationToken)>,
    file_bind: Option<(Uuid, CancellationToken)>,
    files: Vec<RemoteFile>,
    files_context: Option<files::FileContext>,
    file_listing_cancel: Option<CancellationToken>,
    file_preview_cancel: Option<CancellationToken>,
    files_path: String,
    files_request: u64,
    preview_request: u64,
    files_loading: bool,
    path_input: Entity<InputState>,
    preview: Option<String>,
    preview_source: Option<(Arc<Node>, String)>,
    file_editor: Option<FileEditor>,
    file_action: Option<files::FileActionEditor>,
    selected_file: Option<String>,
    transfers: Vec<crate::transfers::Task>,
    transfer_queue: crate::transfers::Queue,
    transfer_updates: tokio::sync::watch::Receiver<crate::transfers::Snapshot>,
    transfer_error: Option<String>,
    forward_bind: Entity<InputState>,
    forward_target: Entity<InputState>,
    forwards: HashMap<Uuid, forwarding::Running>,
    forward_after_save: Option<Uuid>,
    _events_task: Task<()>,
    _prompts_task: Task<()>,
    _filter_subscription: Subscription,
    _palette_subscription: Subscription,
}

impl WorkspaceView {
    pub fn new(
        store: Store,
        runtime: Arc<tokio::runtime::Runtime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (events, receiver) = async_channel::bounded(128);
        let (prompts, prompt_receiver) = async_channel::bounded(16);
        let (auth_prompts, auth_receiver) = async_channel::bounded(8);
        let registry =
            Arc::new(Registry::new(&store.directory, prompts).with_authentication(auth_prompts));
        let (transfer_queue, transfer_updates) = {
            let _guard = runtime.enter();
            crate::transfers::Queue::start(
                store.directory.clone(),
                registry.clone(),
                transfer_queue::configuration(&store.profile),
            )
        };
        let mut transfer_relay_updates = transfer_updates.clone();
        let transfer_events = events.clone();
        let transfer_relay = runtime.spawn(async move {
            while transfer_relay_updates.changed().await.is_ok() {
                if transfer_events.send(BackendEvent::Transfers).await.is_err() {
                    break;
                }
            }
        });
        let auth_events = events.clone();
        let auth_task = runtime.spawn(async move {
            while let Ok(prompt) = auth_receiver.recv().await {
                if auth_events
                    .send(BackendEvent::Authentication(prompt))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let events_task = cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = receiver.recv().await {
                if view
                    .update_in(cx, |view, window, cx| {
                        view.receive(event, window, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let prompts_task = cx.spawn_in(window, async move |view, cx| {
            while let Ok(prompt) = prompt_receiver.recv().await {
                if view
                    .update_in(cx, |view, window, cx| {
                        view.prompts.retain(|prompt| !prompt.answer.is_closed());
                        if !prompt.answer.is_closed() {
                            view.prompts.push_back(prompt);
                            if view.auth_dialog.is_none() {
                                view.modal_focus.focus(window, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::i18n::text("搜索连接")));
        let filter_subscription = cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify());
        let palette_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::i18n::text("搜索操作…")));
        let palette_subscription =
            cx.subscribe(&palette_input, |_, _, _: &InputEvent, cx| cx.notify());
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |view, cx| view.request_quit(window, cx))
                .is_err()
        });
        let path_input = cx.new(|cx| InputState::new(window, cx).default_value("."));
        let data_password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(crate::i18n::text("备份密码 · 至少 8 字节"))
        });
        let forward_bind = cx.new(|cx| InputState::new(window, cx).default_value("127.0.0.1:8080"));
        let forward_target = cx.new(|cx| InputState::new(window, cx).default_value("127.0.0.1:80"));
        let (workspace_writer, mut workspace_errors) = {
            let _guard = runtime.enter();
            crate::workspace::session::Writer::start(store.directory.clone())
        };
        let workspace_events = events.clone();
        let workspace_relay = runtime.spawn(async move {
            while workspace_errors.changed().await.is_ok() {
                let error = workspace_errors.borrow_and_update().clone();
                if let Some(error) = error
                    && workspace_events
                        .send(BackendEvent::Error(error))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });
        let mut view = Self {
            store,
            workspace_writer,
            workspace_checkpoint: None,
            split_sizes: HashMap::new(),
            _workspace_timer: None,
            runtime,
            registry,
            events,
            jobs: vec![auth_task, transfer_relay, workspace_relay],
            tabs: Vec::new(),
            panes: HashMap::new(),
            active_tab: None,
            active_pane: None,
            connecting: Default::default(),
            sidebar: PanelSize::sidebar(238.),
            collapsed_connection_groups: HashSet::new(),
            tool_size: PanelSize::sidebar(340.),
            panel_bounds: Default::default(),
            broadcast: false,
            tool: Tool::Hidden,
            modal: None,
            modal_focus: cx.focus_handle(),
            form: None,
            jump_drafts: Vec::new(),
            connection_removal: None,
            connection_proxy_editor: None,
            overlay: None,
            palette_input,
            settings_page: SettingsPage::General,
            discovered_agents: Vec::new(),
            agent_scan: None,
            agents_scanned: false,
            agent_probes: HashMap::new(),
            skills_import: None,
            record_editor: None,
            integration_editor: None,
            status: None,
            success_notification: Default::default(),
            _success_notification_timer: None,
            configuration_save: None,
            shutting_down: false,
            data_busy: None,
            data_apply: None,
            data_password,
            data_credentials: false,
            data_replace: false,
            data_preview: None,
            filter,
            prompts: VecDeque::new(),
            auth_dialog: None,
            pending_authentication: VecDeque::new(),
            file_connection: None,
            file_connect: None,
            file_bind: None,
            files: Vec::new(),
            files_context: None,
            file_listing_cancel: None,
            file_preview_cancel: None,
            files_path: ".".into(),
            files_request: 0,
            preview_request: 0,
            files_loading: false,
            path_input,
            preview: None,
            preview_source: None,
            file_editor: None,
            file_action: None,
            selected_file: None,
            transfers: Vec::new(),
            transfer_queue,
            transfer_updates,
            transfer_error: None,
            forward_bind,
            forward_target,
            forwards: HashMap::new(),
            forward_after_save: None,
            _events_task: events_task,
            _prompts_task: prompts_task,
            _filter_subscription: filter_subscription,
            _palette_subscription: palette_subscription,
        };
        view.sync_component_theme(cx);
        view.restore_workspace(window, cx);
        view.start_workspace_checkpoints(window, cx);
        cx.defer_in(window, |view, window, cx| view.focus_terminal(window, cx));
        view
    }

    fn launch(&mut self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.jobs.retain(|job| !job.is_finished());
        self.jobs.push(self.runtime.spawn(future));
    }

    fn palette(&self) -> Palette {
        Palette::for_appearance(
            self.form
                .as_ref()
                .and_then(|form| form.appearance)
                .unwrap_or(self.store.profile.preferences.appearance),
        )
    }

    fn sync_component_theme(&self, cx: &mut Context<Self>) {
        use gpui_kit::component::Colorize as _;
        let appearance = self
            .form
            .as_ref()
            .and_then(|form| form.appearance)
            .unwrap_or(self.store.profile.preferences.appearance);
        gpui_kit::component::Theme::change(
            if appearance == Appearance::Light {
                gpui_kit::component::ThemeMode::Light
            } else {
                gpui_kit::component::ThemeMode::Dark
            },
            None,
            cx,
        );
        let p = Palette::for_appearance(appearance);
        let preferences = &self.store.profile.preferences;
        let ui = crate::fonts::FontPair::ui(preferences, cx);
        let terminal = crate::fonts::FontPair::terminal(preferences, cx);
        let theme = cx.global_mut::<gpui_kit::component::Theme>();
        theme.font_family = ui.latin.family;
        theme.font_size = px(preferences.ui_font_size);
        theme.mono_font_family = terminal.latin.family;
        theme.mono_font_size = px(preferences.font_size);
        theme.radius = px(6.);
        theme.radius_lg = px(10.);
        theme.shadow = false;
        theme.focus_ring = false;
        theme.background = rgb(p.base).into();
        theme.foreground = rgb(p.text).into();
        theme.border = rgb(p.border).into();
        theme.input = rgb(p.border).into();
        theme.ring = rgb(p.accent).into();
        theme.caret = rgb(p.accent).into();
        theme.primary = rgb(p.accent).into();
        theme.primary_foreground = rgb(p.base).into();
        theme.secondary = rgb(p.surface).into();
        theme.secondary_foreground = rgb(p.text).into();
        theme.muted = rgb(p.surface).into();
        theme.muted_foreground = rgb(p.muted).into();
        theme.accent = rgb(p.hover).into();
        theme.accent_foreground = rgb(p.text).into();
        theme.button = rgb(p.surface).into();
        theme.button_foreground = rgb(p.text).into();
        theme.button_hover = rgb(p.hover).into();
        theme.button_active = rgb(p.hover).into();
        theme.button_primary = rgb(p.accent).into();
        theme.button_primary_foreground = rgb(p.base).into();
        theme.button_primary_hover = theme.button_primary.mix_oklab(rgb(p.text).into(), 0.08);
        theme.button_primary_active = theme.button_primary.mix_oklab(rgb(p.base).into(), 0.08);
        theme.popover = rgb(p.sidebar).into();
        theme.popover_foreground = rgb(p.text).into();
        theme.colors.list = rgb(p.sidebar).into();
        theme.list_hover = rgb(p.hover).into();
        theme.list_active = rgb(p.hover).into();
        theme.list_active_border = rgb(p.accent).into();
        theme.selection = rgb(p.hover).into();
    }

    fn add_terminal(
        &mut self,
        terminal: Terminal,
        node: Option<Arc<Node>>,
        split: Option<Placement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = Uuid::new_v4();
        let title = node
            .as_ref()
            .map(|node| node.connection.name.clone())
            .unwrap_or_else(|| crate::i18n::text("本地终端").into());
        let source = node
            .as_ref()
            .map(|node| crate::workspace::session::Source::Ssh {
                connection: node.connection.id,
            })
            .unwrap_or_else(|| crate::workspace::session::Source::Local {
                shell: self.store.profile.preferences.shell.clone(),
                directory: self.store.profile.preferences.directory.clone(),
            });
        self.create_pane(id, terminal, node, source, window, cx);
        let split_done = if let Some(target) = split {
            let done = self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == target.tab)
                .is_some_and(|tab| tab.layout.split(target.pane, id, target.axis));
            if done {
                self.active_tab = Some(target.tab);
            }
            done
        } else {
            false
        };
        if !split_done {
            let tab = Tab {
                id: Uuid::new_v4(),
                name: title,
                layout: Layout::Pane(id),
                active_pane: id,
            };
            self.active_tab = Some(tab.id);
            self.tabs.push(tab);
        }
        self.activate_pane(id, cx);
        self.reset_files(cx);
        cx.notify();
    }

    fn create_pane(
        &mut self,
        id: Uuid,
        terminal: Terminal,
        node: Option<Arc<Node>>,
        source: crate::workspace::session::Source,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preferences = self.store.profile.preferences.clone();
        let pane_engine = terminal.engine.clone();
        let file_target = pane_engine.directory_target();
        let view = cx.new(|cx| TerminalView::new(terminal, preferences, window, cx));
        view.update(cx, |view, cx| view.focus(window, cx));
        let observe = cx.observe_in(&view, window, move |workspace, terminal, window, cx| {
            let engine = terminal.read(cx).terminal.engine.clone();
            if let Some((request, direction)) = engine.zmodem_request() {
                let send = direction == crate::terminal::zmodem::Direction::Send;
                let paths = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                    files: send,
                    directories: !send,
                    multiple: send,
                    prompt: Some(
                        crate::i18n::text(if send {
                            "选择 Zmodem 发送文件"
                        } else {
                            "选择 Zmodem 接收目录"
                        })
                        .into(),
                    ),
                });
                cx.spawn_in(window, async move |view, cx| {
                    let selected = match paths.await {
                        Ok(Ok(Some(paths))) => paths,
                        _ => Vec::new(),
                    };
                    if let Err(error) = engine.start_zmodem(request, selected) {
                        engine.cancel_zmodem();
                        let _ = view.update(cx, |view, cx| {
                            view.status = Some(format!("Zmodem：{error:#}"));
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            let directory = terminal.read(cx).snapshot.directory.clone();
            let changed = workspace.panes.get_mut(&id).is_some_and(|pane| {
                let target = pane.engine.directory_target();
                if pane.directory != directory || pane.file_target != target {
                    if pane.file_target != target {
                        pane.file_binding = None;
                    }
                    pane.file_target = target;
                    pane.directory = directory.clone();
                    true
                } else {
                    false
                }
            });
            if changed
                && workspace.active_pane == Some(id)
                && workspace.tool == Tool::Files
                && workspace.file_connection.is_none()
            {
                workspace.reset_files(cx);
                workspace.path_input.update(cx, |input, cx| {
                    input.set_value(workspace.files_path.clone(), window, cx)
                });
            }
            if terminal.read(cx).snapshot.closed
                && let Some(pane) = workspace.panes.get_mut(&id)
            {
                pane.assistant.session.end();
            }
            cx.notify();
        });
        let events = cx.subscribe(&view, move |workspace, _, event: &PaneEvent, cx| {
            match event {
                PaneEvent::Focused => {
                    workspace.activate_pane(id, cx);
                }
                PaneEvent::Input(bytes) if workspace.broadcast => {
                    if let Some(tab) = workspace
                        .tabs
                        .iter()
                        .find(|tab| Some(tab.id) == workspace.active_tab)
                    {
                        for target in tab.layout.panes() {
                            if target != id
                                && let Some(pane) = workspace.panes.get(&target)
                                && let Err(error) =
                                    pane.view.read(cx).terminal.engine.input(bytes.clone())
                            {
                                workspace.status = Some(error.to_string());
                            }
                        }
                    }
                }
                PaneEvent::Paste(text) if workspace.broadcast => {
                    if let Some(tab) = workspace
                        .tabs
                        .iter()
                        .find(|tab| Some(tab.id) == workspace.active_tab)
                    {
                        for target in tab.layout.panes() {
                            if target != id
                                && let Some(pane) = workspace.panes.get(&target)
                                && let Err(error) = pane.view.read(cx).terminal.engine.paste(text)
                            {
                                workspace.status = Some(error.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
            cx.notify();
        });
        self.panes.insert(
            id,
            Pane {
                source,
                engine: pane_engine,
                file_target,
                file_binding: None,
                view,
                node,
                assistant: PaneAssistant::new(
                    id,
                    &self.store.profile.ai_providers,
                    &self.store.profile.agents,
                    window,
                    cx,
                ),
                reconnect: None,
                directory: None,
                dock: PaneDock::default(),
                bounds: Default::default(),
                _observe: observe,
                _events: events,
            },
        );
        self.refresh_reasoning(id, window, cx);
    }

    fn local(&mut self, split: Option<Axis>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_open_terminal(cx) {
            return;
        }
        let split = split.and_then(|axis| self.placement(axis));
        let mut preferences = self.store.profile.preferences.clone();
        if split.is_some()
            && let Some(pane) = self.active_pane.and_then(|id| self.panes.get(&id))
            && matches!(pane.source, crate::workspace::session::Source::Local { .. })
            && let Some(directory) = &pane.directory
            && std::path::Path::new(directory).is_dir()
        {
            preferences.directory = directory.into();
        }
        match Terminal::local(&preferences) {
            Ok(terminal) => {
                self.add_terminal(terminal, None, split, window, cx);
                if let Some(pane) = self.active_pane.and_then(|id| self.panes.get_mut(&id)) {
                    pane.source = crate::workspace::session::Source::Local {
                        shell: preferences.shell,
                        directory: preferences.directory,
                    };
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        };
    }

    fn connect(&mut self, id: Uuid, split: Option<Axis>, cx: &mut Context<Self>) {
        if self
            .store
            .profile
            .connections
            .iter()
            .any(|c| c.id == id && c.kind == crate::config::ConnectionKind::Sftp)
        {
            self.connect_files(id, cx);
            return;
        }
        if !self.can_open_terminal(cx) {
            return;
        }
        let split = split.and_then(|axis| self.placement(axis));
        let Some((request, cancel)) = self.connecting.begin(id, split.map(|target| target.pane))
        else {
            return;
        };
        let registry = self.registry.clone();
        let connections = self.store.profile.connections.clone();
        let proxies = self.store.profile.proxies.clone();
        let preferences = self.store.profile.preferences.clone();
        let events = self.events.clone();
        self.launch(async move {
            let connecting = async {
                let node = registry.connect_using(id, &connections, &proxies).await?;
                let terminal = node.terminal(&preferences).await?;
                Ok((node, terminal))
            };
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = connecting => result,
            };
            let _ = events
                .send(BackendEvent::Connected {
                    id,
                    request,
                    split,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    fn can_open_terminal(&mut self, cx: &mut Context<Self>) -> bool {
        if self.shutting_down {
            return false;
        }
        if self.panes.len() + self.connecting.len() >= 64 {
            self.status = Some(crate::i18n::text("最多同时打开 64 个终端会话").into());
            cx.notify();
            return false;
        }
        true
    }

    fn placement(&self, axis: Axis) -> Option<Placement> {
        Some(Placement {
            tab: self.active_tab?,
            pane: self.active_pane?,
            axis,
        })
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let source = self
            .active_pane
            .and_then(|id| self.panes.get(&id))
            .map(|pane| &pane.source);
        if let Some(crate::workspace::session::Source::Ssh { connection }) = source {
            self.connect(*connection, Some(axis), cx);
        } else {
            self.local(Some(axis), window, cx);
        }
    }

    fn current_node(&self) -> Option<Arc<Node>> {
        self.active_pane
            .and_then(|id| self.panes.get(&id))
            .and_then(|pane| pane.node.clone())
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.current_node() else {
            return;
        };
        let id = node.connection.id;
        let registry = self.registry.clone();
        let events = self.events.clone();
        self.launch(async move {
            if let Err(error) = registry.disconnect(id).await {
                let _ = events.send(BackendEvent::Error(error.to_string())).await;
            }
        });
        cx.notify();
    }

    fn close_pane(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.connecting.cancel_pane(id);
        if let Some(pane) = self.panes.get_mut(&id)
            && let Some((_, cancel)) = pane.reconnect.take()
        {
            cancel.cancel();
        }
        self.panes.remove(&id);
        self.tabs.retain_mut(|tab| {
            if !tab.layout.panes().contains(&id) {
                return true;
            }
            if let Some(layout) = tab.layout.clone().remove(id) {
                tab.layout = layout;
                if tab.active_pane == id
                    && let Some(next) = tab.layout.panes().first()
                {
                    tab.active_pane = *next;
                }
                true
            } else {
                false
            }
        });
        if !self.tabs.iter().any(|tab| Some(tab.id) == self.active_tab) {
            self.active_tab = self.tabs.last().map(|tab| tab.id);
        }
        if self.active_pane == Some(id) || self.active_pane.is_none() {
            self.active_pane = self
                .tabs
                .iter()
                .find(|tab| Some(tab.id) == self.active_tab)
                .map(|tab| tab.active_pane);
        }
        self.focus_terminal(window, cx);
        self.reset_files(cx);
        cx.notify();
    }

    fn receive(&mut self, event: BackendEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.shutting_down
            && !matches!(
                event,
                BackendEvent::ShutdownReady | BackendEvent::ShutdownFailed(_)
            )
        {
            return;
        }
        self.prompts.retain(|prompt| !prompt.answer.is_closed());
        match event {
            BackendEvent::SshAgentProbe {
                form,
                request,
                result,
            } => {
                self.receive_ssh_agent_probe(form, request, result, window, cx);
            }
            BackendEvent::AgentProbe {
                agent,
                request,
                result,
            } => self.receive_agent_probe(agent, request, result),
            BackendEvent::SkillsImported {
                request,
                base,
                result,
            } => self.receive_skills_import(request, base, result, cx),
            BackendEvent::AgentConfiguration {
                pane,
                identity,
                request,
                result,
            } => self.receive_agent_configuration(pane, identity, request, result, cx),
            BackendEvent::LocalAgents { request, agents } => {
                if self.agent_scan == Some(request) {
                    self.agent_scan = None;
                    self.agents_scanned = true;
                    self.discovered_agents = agents;
                }
            }
            BackendEvent::ProviderCatalog {
                editor,
                request,
                apply,
                result,
            } => {
                self.receive_provider_catalog(editor, request, apply, result, cx);
            }
            BackendEvent::DataLoaded { id, result } => {
                if self.data_busy == Some(id) {
                    self.data_busy = None;
                    match result {
                        Ok(preview) => {
                            self.data_preview = Some(preview);
                            self.data_replace = false;
                            self.status = None;
                        }
                        Err(error) => self.status = Some(error.to_string()),
                    }
                }
            }
            BackendEvent::DataExported { id, result } => {
                if self.data_busy == Some(id) {
                    self.data_busy = None;
                    match result {
                        Ok(()) => self.notify_success(crate::i18n::text("加密备份已保存"), cx),
                        Err(error) => self.status = Some(error.to_string()),
                    }
                }
            }
            BackendEvent::ShutdownFailed(error) => {
                self.shutting_down = false;
                self.status = Some(error);
            }
            BackendEvent::ShutdownReady => {
                crate::config::clear_secret_cache();
                cx.quit();
            }
            BackendEvent::AiProgress {
                pane,
                session,
                request,
                text,
            } => {
                if let Some(pane) = self.panes.get_mut(&pane) {
                    pane.assistant.session.progress(session, request, text);
                }
            }
            BackendEvent::AgentPermission {
                pane,
                session,
                permission,
            } => {
                if let Some(pane) = self.panes.get_mut(&pane)
                    && pane.assistant.session.id == session
                    && !pane.assistant.session.ended
                {
                    pane.assistant.permissions.push(permission);
                }
            }
            BackendEvent::ToolResult {
                pane,
                session,
                tool,
                result,
            } => self.receive_tool(pane, session, tool, result, cx),
            BackendEvent::ConfigurationSaved { id, result } => {
                if self.configuration_save != Some(id) {
                    return;
                }
                self.configuration_save = None;
                let applied_data = self.data_apply == Some(id);
                if applied_data {
                    self.data_apply = None;
                }
                if let Some(editor) = &mut self.record_editor
                    && editor.id == id
                {
                    editor.saving = false;
                }
                if let Some(editor) = &mut self.integration_editor
                    && editor.id == id
                {
                    editor.saving = false;
                }
                match result {
                    Ok(profile) => {
                        if applied_data {
                            self.data_preview = None;
                        }
                        let integrations_changed = serde_json::to_value((
                            &self.store.profile.ai_providers,
                            &self.store.profile.mcp_servers,
                            &self.store.profile.agents,
                            &self.store.profile.skills_roots,
                            &self.store.profile.proxies,
                        ))
                        .ok()
                            != serde_json::to_value((
                                &profile.ai_providers,
                                &profile.mcp_servers,
                                &profile.agents,
                                &profile.skills_roots,
                                &profile.proxies,
                            ))
                            .ok();
                        self.store.profile = profile;
                        self.synchronize_forward_rules();
                        if self.forward_after_save == Some(id) {
                            self.forward_after_save = None;
                            self.start_forward(id, cx);
                        }
                        if let Err(error) = self
                            .transfer_queue
                            .configure(transfer_queue::configuration(&self.store.profile))
                        {
                            self.transfer_error = Some(error.to_string());
                        }
                        let preferred_proxy = self
                            .connection_proxy_editor
                            .filter(|(_, proxy)| *proxy == id);
                        if preferred_proxy.is_some() {
                            self.connection_proxy_editor = None;
                        }
                        self.refresh_connection_proxies(preferred_proxy, window, cx);
                        let registry = self.registry.clone();
                        let proxies = self.store.profile.proxies.clone();
                        self.launch(async move {
                            registry.refresh_proxies(proxies).await;
                        });
                        crate::i18n::set_language(self.store.profile.preferences.language);
                        crate::menus::install(cx);
                        for pane in self.panes.values_mut() {
                            if integrations_changed {
                                pane.assistant.session.reset();
                            }
                            pane.view.update(cx, |view, cx| {
                                view.apply_preferences(&self.store.profile.preferences, window, cx);
                            });
                            pane.assistant.command.update(cx, |input, cx| {
                                input.set_placeholder(
                                    crate::i18n::text("多行命令、管道、脚本… Enter 换行"),
                                    window,
                                    cx,
                                )
                            });
                            pane.assistant.prompt.update(cx, |input, cx| {
                                input.set_placeholder(
                                    crate::i18n::text("询问命令，分析此终端的问题…"),
                                    window,
                                    cx,
                                )
                            });
                        }
                        self.filter.update(cx, |input, cx| {
                            input.set_placeholder(crate::i18n::text("搜索连接"), window, cx)
                        });
                        self.palette_input.update(cx, |input, cx| {
                            input.set_placeholder(crate::i18n::text("搜索操作…"), window, cx)
                        });
                        if self.modal == Some(Modal::Connection)
                            && self.form.as_ref().is_some_and(|form| form.id == id)
                        {
                            self.restore_jump_draft(Some(id), window, cx);
                        }
                        self.sync_component_theme(cx);
                        if self
                            .integration_editor
                            .as_ref()
                            .is_some_and(|editor| editor.id == id)
                        {
                            self.integration_editor = None;
                        }
                        if self
                            .record_editor
                            .as_ref()
                            .is_some_and(|editor| editor.id == id)
                        {
                            self.record_editor = None;
                        }
                        self.refresh_provider_choices(window, cx);
                        if self
                            .connection_removal
                            .as_ref()
                            .is_some_and(|removal| removal.id == id)
                        {
                            self.connection_removal = None;
                            self.notify_success(crate::i18n::text("连接及其保存的凭据已删除"), cx);
                        } else {
                            self.notify_success(crate::i18n::text("配置已保存"), cx);
                        }
                        self.focus_terminal(window, cx);
                    }
                    Err(error) => {
                        if let Some(editor) = &mut self.record_editor
                            && editor.id == id
                        {
                            editor.error = Some(format!("{error:#}"));
                        }
                        if let Some(removal) = &mut self.connection_removal
                            && removal.id == id
                        {
                            removal.error = Some(format!("{error:#}"));
                        }
                        if self.forward_after_save == Some(id) {
                            self.forward_after_save = None;
                        }
                        self.status = Some(format!("{error:#}"));
                    }
                }
            }
            BackendEvent::ConnectionProbe {
                form: id,
                request,
                result,
            } => {
                if let Some(form) = &mut self.form
                    && form.id == id
                    && form
                        .connection_probe
                        .as_ref()
                        .is_some_and(|probe| probe.id == request)
                {
                    form.connection_probe = None;
                    form.probe_result = Some(match result {
                        Ok(()) => (
                            true,
                            crate::i18n::text("整条 SSH 路径连接与认证成功").into(),
                        ),
                        Err(error) => (false, format!("{error:#}")),
                    });
                }
            }
            BackendEvent::Reconnected {
                pane: id,
                request,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&id) else {
                    return;
                };
                if pane
                    .reconnect
                    .as_ref()
                    .is_none_or(|(expected, cancel)| *expected != request || cancel.is_cancelled())
                {
                    return;
                }
                pane.reconnect = None;
                match result {
                    Ok((node, terminal)) => {
                        pane.engine = terminal.engine.clone();
                        pane.file_binding = None;
                        pane.file_target = pane.engine.directory_target();
                        let preferences = self.store.profile.preferences.clone();
                        pane.view.update(cx, |view, cx| {
                            if view.snapshot.closed {
                                terminal.engine.inherit_screen(&view.terminal.engine);
                            }
                            *view = TerminalView::new(terminal, preferences, window, cx);
                        });
                        let connection = node.as_ref().map(|node| node.connection.id);
                        pane.node = node;
                        pane.assistant.session = crate::assistant_session::Session::new();
                        if let Some(connection) = connection {
                            self.start_connection_forwards(connection, cx);
                        }
                        self.focus_terminal(window, cx);
                        self.reset_files(cx);
                    }
                    Err(error) => self.status = Some(format!("{error:#}")),
                }
            }
            BackendEvent::Authentication(prompt) => {
                if prompt.answer.is_closed() {
                    return;
                }
                self.pending_authentication
                    .retain(|prompt| !prompt.answer.is_closed());
                if self.pending_authentication.len() < 64 {
                    self.pending_authentication.push_back(prompt);
                } else {
                    let _ = prompt.answer.send(None);
                }
            }
            BackendEvent::Connected {
                id,
                request,
                split,
                result,
            } => {
                if !self.connecting.complete(id, request) || self.shutting_down {
                    return;
                }
                match result {
                    Ok((node, terminal)) => {
                        self.add_terminal(terminal, Some(node), split, window, cx);
                        self.start_connection_forwards(id, cx);
                    }
                    Err(error) => self.status = Some(error.to_string()),
                }
            }
            BackendEvent::SftpBound {
                request,
                pane: id,
                target,
                node,
                result,
            } => {
                if self
                    .file_bind
                    .as_ref()
                    .is_none_or(|(id, cancel)| *id != request || cancel.is_cancelled())
                {
                    return;
                }
                self.file_bind = None;
                if self.active_pane != Some(id)
                    || self
                        .file_connection
                        .as_ref()
                        .is_none_or(|current| current.connection.id != node.connection.id)
                {
                    return;
                }
                if let Some(pane) = self.panes.get_mut(&id)
                    && pane
                        .engine
                        .directory_target()
                        .is_some_and(|t| t.id == target)
                {
                    match result {
                        Ok(()) => {
                            pane.file_binding = Some((target, node));
                            self.file_connection = None;
                            self.reset_files(cx);
                        }
                        Err(error) => self.status = Some(format!("{error:#}")),
                    }
                }
            }
            BackendEvent::SftpConnected { request, result } => {
                if self
                    .file_connect
                    .as_ref()
                    .is_some_and(|(id, _)| *id == request)
                {
                    self.file_connect = None;
                    match result {
                        Ok(node) => {
                            self.files_path = node.connection.initial_directory.clone();
                            self.file_connection = Some(node);
                            self.tool = Tool::Files;
                            self.tool_size.open = true;
                            self.path_input.update(cx, |input, cx| {
                                input.set_value(self.files_path.clone(), window, cx)
                            });
                            self.reset_files(cx);
                        }
                        Err(error) => self.status = Some(format!("{error:#}")),
                    }
                }
            }
            BackendEvent::FileActionDone { id, result } => {
                if self
                    .file_action
                    .as_ref()
                    .is_some_and(|editor| editor.id == id)
                {
                    match result {
                        Ok(()) => {
                            self.file_action = None;
                            self.load_files(cx);
                        }
                        Err(error) => {
                            if let Some(editor) = &mut self.file_action {
                                editor.busy = false;
                                editor.error = Some(format!("{error:#}"));
                            }
                        }
                    }
                }
            }
            BackendEvent::Files { request, result }
                if request == self.files_request && self.files_current() =>
            {
                self.files_loading = false;
                match result {
                    Ok(files) => self.files = files,
                    Err(error) => self.status = Some(error.to_string()),
                }
            }
            BackendEvent::Files { .. } => {}
            BackendEvent::Preview {
                request,
                node,
                path,
                result,
            } if request == self.preview_request && self.files_current() => match result {
                Ok(text) => {
                    self.preview = Some(text);
                    self.preview_source = Some((node, path));
                }
                Err(error) => self.status = Some(error.to_string()),
            },
            BackendEvent::Preview { .. } => {}
            BackendEvent::FileSaved {
                id,
                content,
                result,
            } => {
                if let Some(editor) = &mut self.file_editor
                    && editor.id == id
                {
                    editor.saving = false;
                    match result {
                        Ok(()) => {
                            editor.original = content.clone();
                            self.preview = Some(content);
                            self.notify_success(crate::i18n::text("文件已保存"), cx);
                        }
                        Err(error) => self.status = Some(format!("{error:#}")),
                    }
                }
            }
            BackendEvent::Transfers => {
                let snapshot = self.transfer_updates.borrow_and_update().clone();
                let completed = snapshot
                    .tasks
                    .iter()
                    .filter(|t| t.state == crate::transfers::State::Complete)
                    .count();
                let old_completed = self
                    .transfers
                    .iter()
                    .filter(|t| t.state == crate::transfers::State::Complete)
                    .count();
                self.transfers = snapshot.tasks;
                self.transfer_error = snapshot.error;
                if completed > old_completed && self.tool == Tool::Files {
                    self.load_files(cx);
                }
            }
            BackendEvent::Ai {
                pane,
                session,
                request,
                result,
            } => self.receive_ai(pane, session, request, result, cx),
            BackendEvent::Forward {
                rule,
                request,
                result,
            } => self.receive_forward(rule, request, result),
            BackendEvent::Error(error) => self.status = Some(error),
        }
        self.advance_authentication(window, cx);
    }

    fn reset_files(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.file_listing_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.file_preview_cancel.take() {
            cancel.cancel();
        }
        self.preview_source = None;
        self.preview_request = self.preview_request.wrapping_add(1);
        self.files_request = self.files_request.wrapping_add(1);
        self.files.clear();
        self.preview = None;
        self.selected_file = None;
        self.files_loading = false;
        if self.tool == Tool::Files {
            if self.file_connection.is_none()
                && let Some(directory) = self
                    .active_pane
                    .and_then(|id| self.panes.get(&id))
                    .and_then(|pane| pane.view.read(cx).snapshot.directory.clone())
            {
                self.files_path = directory;
            }
            self.load_files(cx);
        }
    }

    fn load_files(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.file_listing_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.file_preview_cancel.take() {
            cancel.cancel();
        }
        self.files_request = self.files_request.wrapping_add(1);
        self.files_context = self.file_context();
        let Some(node) = self.file_node() else {
            self.files.clear();
            self.preview = None;
            self.preview_source = None;
            self.selected_file = None;
            self.files_loading = false;
            return;
        };
        let request = self.files_request;
        self.files_loading = true;
        self.files.clear();
        self.preview = None;
        self.preview_source = None;
        self.selected_file = None;
        self.preview_request = self.preview_request.wrapping_add(1);
        let path = self.files_path.clone();
        let events = self.events.clone();
        let cancel = CancellationToken::new();
        self.file_listing_cancel = Some(cancel.clone());
        self.launch(async move {let result=tokio::select!{()=cancel.cancelled()=>return,()=node.cancel.cancelled()=>Err(anyhow::anyhow!(crate::i18n::text("连接已断开"))),result=node.list(&path)=>result};let _=events.send(BackendEvent::Files{request,result}).await;});
        cx.notify();
    }

    fn open_file(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.files_loading || !self.files_current() {
            return;
        }
        let Some(file) = self.files.get(index).cloned() else {
            return;
        };
        let path = remote_path(&self.files_path, &file.name);
        if file.directory {
            self.files_path = path;
            self.path_input.update(cx, |input, cx| {
                input.set_value(self.files_path.clone(), window, cx)
            });
            self.preview = None;
            self.selected_file = None;
            self.load_files(cx);
        } else {
            self.preview_request = self.preview_request.wrapping_add(1);
            let request = self.preview_request;
            self.selected_file = Some(path.clone());
            self.preview = None;
            self.preview_source = None;
            if let Some(node) = self.file_node() {
                let events = self.events.clone();
                if let Some(cancel) = self.file_preview_cancel.take() {
                    cancel.cancel();
                }
                let cancel = CancellationToken::new();
                self.file_preview_cancel = Some(cancel.clone());
                self.launch(async move {
                    let result = tokio::select! { () = cancel.cancelled() => return, result = node.read_text(&path) => result };
                    let _ = events
                        .send(BackendEvent::Preview {
                            request,
                            node,
                            path,
                            result,
                        })
                        .await;
                });
            }
        }
    }

    fn edit_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.files_current() {
            return;
        }
        if let Some(original) = &self.preview
            && let Some((node, path)) = &self.preview_source
        {
            if original.len() > crate::sftp_edit::MAX_EDIT_BYTES {
                self.status = Some(crate::i18n::text("编辑只支持 512 KiB 以内的文本文件").into());
                cx.notify();
                return;
            }
            let input = cx.new(|cx| TextareaState::new(window, cx).default_value(original.clone()));
            input.update(cx, |input, cx| input.focus(window, cx));
            self.file_editor = Some(FileEditor {
                id: Uuid::new_v4(),
                node: node.clone(),
                path: path.clone(),
                original: original.clone(),
                input,
                saving: false,
            });
            cx.notify();
        }
    }

    fn save_file(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.file_editor else {
            return;
        };
        if editor.saving {
            return;
        }
        editor.saving = true;
        let (id, node, path, original, content) = (
            editor.id,
            editor.node.clone(),
            editor.path.clone(),
            editor.original.clone(),
            editor.input.read(cx).value().to_string(),
        );
        let events = self.events.clone();
        self.launch(async move {
            let result = node.save_text(&path, &original, &content).await;
            let _ = events
                .send(BackendEvent::FileSaved {
                    id,
                    content,
                    result,
                })
                .await;
        });
        cx.notify();
    }

    fn transfer(
        &mut self,
        node: Arc<Node>,
        local: PathBuf,
        remote: String,
        upload: bool,
        cx: &mut Context<Self>,
    ) {
        let direction = if upload {
            crate::transfers::Direction::Upload
        } else {
            crate::transfers::Direction::Download
        };
        if let Err(error) =
            self.transfer_queue
                .enqueue(node.connection.id, local, remote, direction)
        {
            self.transfer_error = Some(format!("{error:#}"));
        }
        cx.notify();
    }

    fn upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.file_node() else {
            return;
        };
        let directory = self.files_path.clone();
        let prompt = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some(crate::i18n::text("选择上传文件或目录").into()),
        });
        cx.spawn_in(window, async move |view, cx| match prompt.await {
            Ok(Ok(Some(paths))) => {
                for path in paths {
                    let _ = view.update(cx, |view, cx| {
                        if let Some(name) = path.file_name() {
                            let remote = remote_path(&directory, &name.to_string_lossy());
                            view.transfer(node.clone(), path, remote, true, cx);
                        }
                    });
                }
            }
            Ok(Err(error)) => {
                let _ = view.update(cx, |view, cx| {
                    view.status = Some(error.to_string());
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    fn download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.file_node() else {
            return;
        };
        if !self.files_current() {
            return;
        }
        let remote = self
            .selected_file
            .clone()
            .unwrap_or_else(|| self.files_path.clone());
        self.download_path(node, remote, window, cx);
    }

    fn download_path(
        &mut self,
        node: Arc<Node>,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = remote.rsplit('/').next().unwrap_or("download");
        let prompt = cx.prompt_for_new_path(&self.store.profile.preferences.directory, Some(name));
        cx.spawn_in(window, async move |view, cx| match prompt.await {
            Ok(Ok(Some(path))) => {
                let _ = view.update(cx, |view, cx| view.transfer(node, path, remote, false, cx));
            }
            Ok(Err(error)) => {
                let _ = view.update(cx, |view, cx| {
                    view.status = Some(error.to_string());
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }

    fn settings_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.data_busy.is_some() || self.shutting_down {
            return;
        }
        self.record_editor = None;
        self.settings_page = SettingsPage::General;
        let p = &self.store.profile.preferences;
        self.form = Some(Form::new(
            &[
                ("size", &p.font_size.to_string()),
                ("ui_size", &p.ui_font_size.to_string()),
                ("agent_size", &p.agent_font_size.to_string()),
                ("shell", &p.shell.to_string_lossy()),
                ("directory", &p.directory.to_string_lossy()),
                ("model", &p.ai_model),
                ("endpoint", &p.ai_endpoint),
                ("ai_key", ""),
                ("scrollback", &p.scrollback.to_string()),
                ("transfer_limit", &p.transfer_limit_kib.to_string()),
                ("transfer_concurrency", &p.transfer_concurrency.to_string()),
                ("line_height", &p.terminal.line_height.to_string()),
                ("letter_spacing", &p.terminal.letter_spacing.to_string()),
                (
                    "padding_horizontal",
                    &p.terminal.padding_horizontal.to_string(),
                ),
                ("padding_vertical", &p.terminal.padding_vertical.to_string()),
                ("cursor_width", &p.terminal.cursor_width.to_string()),
                ("cursor_blink_ms", &p.terminal.cursor_blink_ms.to_string()),
                ("scroll_speed", &p.terminal.scroll_speed.to_string()),
            ],
            window,
            cx,
        ));
        self.modal = Some(Modal::Settings);
        if let Some(form) = &mut self.form {
            form.appearance = Some(self.store.profile.preferences.appearance);
            form.preferences = Some(self.store.profile.preferences.clone());
            form.fonts = Some(settings::FontEditor::new(p, window, cx));
            form.shell = Some(settings::ShellEditor::new(&p.shell, window, cx));
            form.terminal = Some(settings::TerminalEditor::new(&p.terminal, window, cx));
            form.theme = Some(settings::ThemeEditor::new(p.appearance, window, cx));
            form.language = Some(cx.new(|cx| {
                crate::controls::select_state(
                    vec![
                        gpui_kit::SharedString::from("简体中文"),
                        gpui_kit::SharedString::from("English"),
                    ],
                    Some(IndexPath::new(
                        if p.language == crate::i18n::Language::Chinese {
                            0
                        } else {
                            1
                        },
                    )),
                    window,
                    cx,
                )
            }));
            if let Some(language) = &form.language {
                language.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    fn save_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.configuration_save.is_some() || self.data_busy.is_some() || self.shutting_down {
            return;
        }
        let result = (|| -> Result<_> {
            let form = self
                .form
                .as_ref()
                .context(crate::i18n::text("设置表单不存在"))?;
            let mut p = form
                .preferences
                .clone()
                .unwrap_or_else(|| self.store.profile.preferences.clone());
            p.appearance = form.appearance.unwrap_or(p.appearance);
            form.fonts
                .as_ref()
                .context("font selectors are not initialized")?
                .apply(&mut p, cx)?;
            form.terminal
                .as_ref()
                .context("terminal selectors are not initialized")?
                .apply(&mut p.terminal, form, cx)?;
            p.font_size = form.value("size", cx).parse()?;
            p.ui_font_size = form.value("ui_size", cx).parse()?;
            p.agent_font_size = form.value("agent_size", cx).parse()?;
            ensure!(
                (10. ..=24.).contains(&p.agent_font_size),
                crate::i18n::text("Agent 字号必须介于 10 和 24 之间")
            );
            ensure!(
                (10. ..=24.).contains(&p.ui_font_size),
                crate::i18n::text("界面字号必须介于 10 和 24 之间")
            );
            ensure!(
                (8. ..=32.).contains(&p.font_size),
                crate::i18n::text("字体大小必须介于 8 和 32 之间")
            );
            p.shell = form
                .shell
                .as_ref()
                .context("shell selector is not initialized")?
                .selected(cx)?;
            p.directory = form.value("directory", cx).into();
            ensure!(p.directory.is_dir(), crate::i18n::text("工作目录不存在"));
            p.scrollback = form.value("scrollback", cx).parse()?;
            p.transfer_limit_kib = form.value("transfer_limit", cx).parse()?;
            p.transfer_concurrency = form.value("transfer_concurrency", cx).parse()?;
            ensure!(
                p.scrollback <= 100_000,
                crate::i18n::text("滚动历史最多为 100000 行")
            );
            p.language = if form
                .language
                .as_ref()
                .and_then(|state| state.read(cx).selected_index(cx))
                .is_some_and(|index| index.row == 1)
            {
                crate::i18n::Language::English
            } else {
                crate::i18n::Language::Chinese
            };
            Ok(p)
        })();
        match result {
            Ok(preferences) => {
                let mut profile = self.store.profile.clone();
                profile.preferences = preferences;
                if let Err(error) = self.persist_profile(Uuid::new_v4(), profile, Vec::new(), cx) {
                    self.status = Some(error.to_string());
                }
            }
            Err(error) => self.status = Some(error.to_string()),
        }
        cx.notify();
    }

    fn show_pane_history(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let tool = history_tool(self.tool, self.active_pane, id, self.tool_size.open);
        self.activate_pane(id, cx);
        self.tool = tool;
        self.tool_size.open = true;
        cx.notify();
    }

    fn switch_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = if self.tool == tool {
            Tool::Hidden
        } else {
            tool
        };
        self.tool_size.open = true;
        if self.tool == Tool::Files {
            self.load_files(cx);
        }
        cx.notify();
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some()
            || self.record_editor.is_some()
            || self.integration_editor.is_some()
            || self.connection_removal.is_some()
            || self.file_editor.is_some()
            || self.file_action.is_some()
            || self.overlay.is_some()
            || !self.prompts.is_empty()
            || self
                .auth_dialog
                .as_ref()
                .is_some_and(|dialog| !dialog.prompt.answer.is_closed())
        {
            return;
        }
        if let Some(pane) = self.active_pane.and_then(|id| self.panes.get(&id)) {
            pane.view
                .update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn render_layout(&self, layout: &Layout, cx: &mut Context<Self>) -> AnyElement {
        match layout {
            Layout::Pane(id) => {
                let Some(pane) = self.panes.get(id) else {
                    return div().into_any_element();
                };
                let id = *id;
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_pane_header(id, cx))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(panels::measure(
                                pane.bounds.clone(),
                                cx.entity().downgrade(),
                                Some(id),
                            ))
                            .child(div().flex_1().min_h_0().min_w_0().child(pane.view.clone()))
                            .child(self.render_pane_dock(id, cx)),
                    )
                    .into_any_element()
            }
            Layout::Split {
                id,
                axis,
                first,
                second,
            } => {
                let group = if *axis == Axis::Vertical {
                    v_resizable(format!("split-{id}"))
                } else {
                    h_resizable(format!("split-{id}"))
                };
                let minimum = if *axis == Axis::Horizontal {
                    px(240.)
                } else {
                    px(100.)
                };
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        group
                            .on_resize({
                                let owner = cx.entity().downgrade();
                                let id = *id;
                                move |state, _, cx| {
                                    let values = state.read(cx).sizes();
                                    if values.len() == 2 {
                                        let sizes = [f32::from(values[0]), f32::from(values[1])];
                                        let _ = owner.update(cx, |view, _| {
                                            view.split_sizes.insert(id, sizes);
                                        });
                                    }
                                }
                            })
                            .child(
                                resizable_panel()
                                    .when_some(self.split_sizes.get(id), |panel, sizes| {
                                        panel.size(px(sizes[0]))
                                    })
                                    .size_range(minimum..gpui_kit::Pixels::MAX)
                                    .child(self.render_layout(first, cx)),
                            )
                            .child(
                                resizable_panel()
                                    .when_some(self.split_sizes.get(id), |panel, sizes| {
                                        panel.size(px(sizes[1]))
                                    })
                                    .size_range(minimum..gpui_kit::Pixels::MAX)
                                    .child(self.render_layout(second, cx)),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        let width = self.sidebar.extent(self.sidebar_limit(PanelEdge::Left));
        let compact = width < 225.;
        let stacked = cfg!(target_os = "macos") && width < 196.;
        let query = self.filter.read(cx).value().to_string();
        div()
            .flex()
            .flex_col()
            .relative()
            .w(px(self.sidebar.extent(self.sidebar_limit(PanelEdge::Left))))
            .min_h_0()
            .overflow_hidden()
            .flex_shrink_0()
            .text_size(px(self.store.profile.preferences.ui_font_size.max(14.)))
            .bg(rgb(p.sidebar))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(panels::HEADER_BAR + if stacked { 32. } else { 0. }))
                    .flex_shrink_0()
                    .px_3()
                    .when(stacked, |header| header.pt(px(32.)))
                    .when(cfg!(target_os = "macos") && !stacked, |header| {
                        header.pl(px(82.))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .children((!compact).then(|| {
                                Icon::new(IconName::Terminal)
                                    .size_5()
                                    .text_color(rgb(p.accent))
                            }))
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_size(px(if compact { 16. } else { 18. }))
                                    .text_ellipsis()
                                    .child("Nutty"),
                            ),
                    )
                    .child(
                        icon_button(
                            "new-connection",
                            IconName::Plus,
                            crate::i18n::text("新建连接"),
                        )
                        .on_click(
                            cx.listener(|view, _, window, cx| {
                                view.connection_form(None, window, cx)
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_3()
                    .child(crate::controls::input(&self.filter, cx).w_full()),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_size(px(
                        (self.store.profile.preferences.ui_font_size - 2.).max(12.)
                    ))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text("工作区")),
            )
            .child(
                div()
                    .id("local-session")
                    .mx_2()
                    .px_3()
                    .py_3()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(p.hover)))
                    .on_click(cx.listener(|view, _, window, cx| view.local(None, window, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Icon::new(IconName::SquareTerminal)
                                    .size_5()
                                    .text_color(rgb(p.accent)),
                            )
                            .child(crate::i18n::text("本地终端")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .pt_5()
                    .pb_2()
                    .text_size(px(
                        (self.store.profile.preferences.ui_font_size - 2.).max(12.)
                    ))
                    .text_color(rgb(p.muted))
                    .child(crate::i18n::text("连接"))
                    .child(self.store.profile.connections.len().to_string()),
            )
            .child(
                div()
                    .id("connection-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .children(self.render_connection_groups(&query, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(panels::FOOTER_BAR))
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(
                        crate::controls::button("settings")
                            .ghost()
                            .accessibility_label(crate::i18n::text("设置"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .w_full()
                                    .justify_center()
                                    .child(Icon::new(IconName::Settings2).size_5())
                                    .child(crate::i18n::text("设置")),
                            )
                            .w_full()
                            .on_click(
                                cx.listener(|view, _, window, cx| view.settings_form(window, cx)),
                            ),
                    ),
            )
            .child(panels::handle("sidebar-resize".into(), PanelEdge::Left))
            .into_any_element()
    }

    fn render_tool(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette();
        let title = match self.tool {
            Tool::History => crate::i18n::text("命令历史"),
            Tool::Files => crate::i18n::text("文件"),
            Tool::Forwarding => crate::i18n::text("端口转发"),
            Tool::Hidden => "",
        };
        let content = match self.tool {
            Tool::History => self.render_history(cx),
            Tool::Files => {
                let connected = self.file_node().is_some();
                let content = div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .p_3()
                    .child(self.render_file_connections(cx));
                if !connected {
                    content
                        .child(
                            div()
                                .text_color(rgb(p.muted))
                                .text_size(px(12.))
                                .child(crate::i18n::text("连接 SSH 后浏览远程文件")),
                        )
                        .child(self.render_transfer_queue(cx))
                        .into_any_element()
                } else {
                    content
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .items_center()
                                    .child(crate::controls::input(&self.path_input, cx).w_full())
                                    .child(
                                        icon_button(
                                            "go-path",
                                            IconName::ArrowRight,
                                            crate::i18n::text("转到目录"),
                                        )
                                        .on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.files_path =
                                                    view.path_input.read(cx).value().to_string();
                                                view.load_files(cx);
                                            }),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .flex()
                                            .gap_1()
                                            .child(
                                                icon_button(
                                                    "parent-path",
                                                    IconName::ArrowUp,
                                                    crate::i18n::text("上级目录"),
                                                )
                                                .on_click(cx.listener(|view, _, window, cx| {
                                                    view.files_path =
                                                        format!("{}/..", view.files_path);
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
                                                    "refresh-files",
                                                    IconName::RefreshCw,
                                                    crate::i18n::text("刷新"),
                                                )
                                                .on_click(cx.listener(|view, _, _, cx| {
                                                    view.load_files(cx)
                                                })),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap_1()
                                            .child(icon_button("new-remote-directory", IconName::FolderPlus, crate::i18n::text("新建文件夹"))
                                                .on_click(cx.listener(|view, _, window, cx| view.open_file_action(None, files::FileAction::Directory, window, cx))))
                                            .child(
                                                icon_button(
                                                    "upload-file",
                                                    IconName::Upload,
                                                    crate::i18n::text("上传"),
                                                )
                                                .on_click(cx.listener(|view, _, window, cx| {
                                                    view.upload(window, cx)
                                                })),
                                            )
                                            .child(
                                                icon_button(
                                                    "download-file",
                                                    IconName::Download,
                                                    crate::i18n::text("下载选中文件"),
                                                )
                                                .on_click(cx.listener(|view, _, window, cx| {
                                                    view.download(window, cx)
                                                })),
                                            ),
                                    ),
                            )
                            .child(self.render_file_bookmarks(cx))
                            .children(self.files_loading.then(|| {
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(p.muted))
                                    .child(crate::i18n::text("读取目录…"))
                            }))
                            .child(
                                uniform_list(
                                    "remote-files",
                                    self.files.len(),
                                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                                        let p = view.palette();
                                        range
                                            .map(|index| {
                                                let file = &view.files[index];
                                                let workspace = cx.entity().downgrade();
                                                let expected_name = file.name.clone();
                                                let expected_directory = view.files_path.clone();
                                                div()
                                                    .id(index)
                                                    .h(px(36.))
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .px_2()
                                                    .rounded_md()
                                                    .cursor_pointer()
                                                    .hover(|style| style.bg(rgb(p.hover)))
                                                    .on_click(cx.listener(
                                                        move |view, _, window, cx| {
                                                            view.open_file(index, window, cx)
                                                        },
                                                    ))
                                                    .child(
                                                        Icon::new(if file.directory {
                                                            IconName::Folder
                                                        } else {
                                                            IconName::FileText
                                                        })
                                                        .size_4()
                                                        .text_color(rgb(if file.directory {
                                                            p.accent
                                                        } else {
                                                            p.muted
                                                        })),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .text_size(px(12.))
                                                            .child(file.name.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(10.))
                                                            .text_color(rgb(p.muted))
                                                            .child(if file.directory {
                                                                String::new()
                                                            } else {
                                                                format_bytes(file.size)
                                                            }),
                                                    )
                                                    .child(icon_button(format!("remote-file-menu-{index}"), IconName::Ellipsis, crate::i18n::text("文件操作"))
                                                        .dropdown_menu(move |mut menu, _, _| {
                                                            let download_workspace = workspace.clone();
                                                            let download_name = expected_name.clone();
                                                            let download_directory = expected_directory.clone();
                                                            menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(crate::i18n::text("下载文件或目录")).icon(IconName::Download).on_click(move |_, window, cx| {
                                                                let _ = download_workspace.update(cx, |view, cx| {
                                                                    if view.files_current() && view.files_path == download_directory && view.files.get(index).is_some_and(|f| f.name == download_name)
                                                                        && let Some(node) = view.file_node() {
                                                                        view.download_path(node, remote_path(&download_directory, &download_name), window, cx);
                                                                    }
                                                                });
                                                            }));
                                                            for (action, label, icon) in [
                                                                (files::FileAction::Rename, "重命名", IconName::SquarePen),
                                                                (files::FileAction::Permissions, "权限", IconName::Shield),
                                                                (files::FileAction::Remove, "删除", IconName::Trash),
                                                            ] {
                                                                let workspace = workspace.clone();
                                                                let expected_name = expected_name.clone();
                                                                let expected_directory = expected_directory.clone();
                                                                menu = menu.item(gpui_kit::component::menu::PopupMenuItem::new(crate::i18n::text(label)).icon(icon).on_click(move |_, window, cx| {
                                                                    let _ = workspace.update(cx, |view, cx| {
                                                                        if view.files_path == expected_directory && view.files.get(index).is_some_and(|f| f.name == expected_name) {
                                                                            view.open_file_action(Some(index), action, window, cx);
                                                                        }
                                                                    });
                                                                }));
                                                            }
                                                            menu
                                                        }))
                                            })
                                            .collect()
                                    }),
                                )
                                .flex_1()
                                .min_h_0(),
                            )
                            .children(self.preview.as_ref().map(|text| {
                                div()
                                    .id("file-preview")
                                    .max_h(px(220.))
                                    .overflow_y_scroll()
                                    .p_3()
                                    .rounded_md()
                                    .bg(rgb(p.surface))
                                    .font(
                                        crate::fonts::FontPair::terminal(
                                            &self.store.profile.preferences,
                                            cx,
                                        )
                                        .latin,
                                    )
                                    .text_size(px(11.))
                                    .child(text.clone())
                            }))
                            .children(self.preview.is_some().then(|| {
                                crate::controls::button("edit-remote-file")
                                    .ghost()
                                    .label(crate::i18n::text("编辑文件"))
                                    .on_click(
                                        cx.listener(|view, _, window, cx| {
                                            view.edit_file(window, cx)
                                        }),
                                    )
                            }))
                            .child(self.render_transfer_queue(cx))
                            .into_any_element()
                }
            }
            Tool::Forwarding => div()
                .flex()
                .flex_col()
                .flex_1()
                .p_4()
                .gap_4()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text("监听地址")),
                )
                .child(crate::controls::input(&self.forward_bind, cx).w_full())
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text("远程目标")),
                )
                .child(crate::controls::input(&self.forward_target, cx).w_full())
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            crate::controls::button("start-local-forward")
                                .primary()
                                .label(crate::i18n::text("本地转发"))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.forward(ForwardMode::Local, cx)
                                })),
                        )
                        .child(
                            crate::controls::button("start-socks-forward")
                                .ghost()
                                .label("SOCKS5")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.forward(ForwardMode::Dynamic, cx)
                                })),
                        )
                        .child(
                            crate::controls::button("start-remote-forward")
                                .ghost()
                                .label(crate::i18n::text("远程转发"))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.forward(ForwardMode::Remote, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(crate::i18n::text("创建并保存规则，可独立启停")),
                )
                .child(self.render_forward_rules(cx))
                .into_any_element(),
            Tool::Hidden => div().into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .relative()
            .w(px(self
                .tool_size
                .extent(self.sidebar_limit(PanelEdge::Right))))
            .overflow_hidden()
            .flex_shrink_0()
            .min_h_0()
            .bg(rgb(p.sidebar))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(panels::HEADER_BAR))
                    .flex_shrink_0()
                    .px_4()
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .child(div().text_size(px(12.)).child(title))
                    .child(
                        icon_button("close-tool", IconName::X, crate::i18n::text("收起面板"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.tool = Tool::Hidden;
                                cx.notify();
                            })),
                    ),
            )
            .child(content)
            .child(panels::handle("tool-resize".into(), PanelEdge::Right))
            .into_any_element()
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.palette();
        if let Some(dialog) = &self.auth_dialog
            && !dialog.prompt.answer.is_closed()
        {
            return Some(
                modal_container(p)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .p_6()
                            .w(px(520.))
                            .max_w(gpui_kit::relative(0.95))
                            .max_h(gpui_kit::relative(0.9))
                            .rounded_xl()
                            .bg(rgb(p.sidebar))
                            .border_1()
                            .border_color(rgb(p.border))
                            .child(div().text_size(px(18.)).child(
                                if dialog.prompt.name.is_empty() {
                                    crate::i18n::text("SSH 身份认证").into()
                                } else {
                                    dialog.prompt.name.clone()
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(p.muted))
                                    .child(dialog.prompt.host.clone()),
                            )
                            .child(
                                div()
                                    .id("ssh-authentication-scroll")
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(dialog.prompt.instructions.clone())
                                    .children(
                                        dialog.prompt.prompts.iter().zip(&dialog.fields).map(
                                            |(prompt, input)| {
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap_2()
                                                    .child(prompt.prompt.clone())
                                                    .child(
                                                        crate::controls::input(input, cx).w_full(),
                                                    )
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        crate::controls::button("cancel-auth")
                                            .ghost()
                                            .label(crate::i18n::text("取消"))
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.cancel_authentication(window, cx);
                                            })),
                                    )
                                    .child(
                                        crate::controls::button("submit-auth")
                                            .primary()
                                            .label(crate::i18n::text("继续"))
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.answer_authentication(window, cx);
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if let Some(prompt) = self
            .prompts
            .iter()
            .find(|prompt| !prompt.answer.is_closed())
        {
            return Some(
                modal_container(p)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_5()
                            .p_6()
                            .w(px(520.))
                            .rounded_xl()
                            .bg(rgb(p.sidebar))
                            .border_1()
                            .border_color(rgb(p.border))
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .child(crate::i18n::text("确认服务器身份")),
                            )
                            .child(prompt.host.clone())
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
                                    .text_color(rgb(p.accent))
                                    .child(prompt.fingerprint.clone()),
                            )
                            .child(
                                div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                                    crate::i18n::text("通过可信来源核对指纹后再信任此服务器。"),
                                ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        crate::controls::button("reject-host")
                                            .ghost()
                                            .label(crate::i18n::text("取消"))
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.prompts
                                                    .retain(|prompt| !prompt.answer.is_closed());
                                                if let Some(prompt) = view.prompts.pop_front() {
                                                    let _ = prompt.answer.send(false);
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        crate::controls::button("trust-host")
                                            .primary()
                                            .label(crate::i18n::text("信任并连接"))
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.prompts
                                                    .retain(|prompt| !prompt.answer.is_closed());
                                                if let Some(prompt) = view.prompts.pop_front() {
                                                    let _ = prompt.answer.send(true);
                                                }
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if self.connection_removal.is_some() {
            return Some(self.render_connection_removal(cx));
        }
        if self.integration_editor.is_some() {
            return Some(self.render_integration_editor(cx));
        }
        if self.record_editor.is_some() {
            return Some(self.render_record_editor(cx));
        }
        if self.file_action.is_some() {
            return Some(self.render_file_action(cx));
        }
        if let Some(editor) = &self.file_editor {
            return Some(
                modal_container(p)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .p_6()
                            .w(px(760.))
                            .rounded_xl()
                            .bg(rgb(p.sidebar))
                            .border_1()
                            .border_color(rgb(p.border))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .child(crate::i18n::text("编辑远程文件")),
                            )
                            .child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(
                                format!("{} · {}", editor.node.connection.name, editor.path),
                            ))
                            .child(
                                crate::controls::textarea(&editor.input, cx)
                                    .w_full()
                                    .h(px(380.))
                                    .disabled(editor.saving),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        crate::controls::button("close-editor")
                                            .ghost()
                                            .label(
                                                if editor.input.read(cx).value().as_str()
                                                    == editor.original
                                                {
                                                    crate::i18n::text("关闭")
                                                } else {
                                                    crate::i18n::text("放弃修改")
                                                },
                                            )
                                            .disabled(editor.saving)
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.file_editor = None;
                                                view.focus_terminal(window, cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        crate::controls::button("save-remote-file")
                                            .primary()
                                            .label(if editor.saving {
                                                crate::i18n::text("保存中…")
                                            } else {
                                                crate::i18n::text("保存")
                                            })
                                            .disabled(editor.saving)
                                            .on_click(
                                                cx.listener(|view, _, _, cx| view.save_file(cx)),
                                            ),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        let modal = self.modal?;
        let form = self.form.as_ref()?;
        let content = match modal {
            Modal::Connection => self.render_connection_form(form, cx),
            Modal::Settings => self.render_settings_page(form, cx),
        };
        Some(
            modal_container(p)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w(px(if modal == Modal::Settings {
                            1000.
                        } else {
                            760.
                        }))
                        .max_w(gpui_kit::relative(0.95))
                        .h(gpui_kit::relative(0.88))
                        .max_h(gpui_kit::relative(0.9))
                        .min_h_0()
                        .overflow_hidden()
                        .rounded_xl()
                        .border_1()
                        .border_color(rgb(p.border))
                        .bg(rgb(p.sidebar))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .items_center()
                                .px_6()
                                .py_4()
                                .border_b_1()
                                .border_color(rgb(p.border))
                                .child(div().text_size(px(17.)).child(
                                    if modal == Modal::Connection && !self.jump_drafts.is_empty() {
                                        crate::i18n::text("跳板机设置")
                                    } else if modal == Modal::Connection {
                                        crate::i18n::text("连接设置")
                                    } else {
                                        crate::i18n::text("设置")
                                    },
                                ))
                                .child(
                                    icon_button(
                                        "close-modal",
                                        IconName::X,
                                        crate::i18n::text("关闭"),
                                    )
                                    .on_click(cx.listener(
                                        |view, _, window, cx| {
                                            if view.configuration_save.is_some()
                                                || view.data_busy.is_some()
                                            {
                                                return;
                                            }
                                            if view.modal == Some(Modal::Connection) {
                                                view.restore_jump_draft(None, window, cx);
                                            } else {
                                                view.modal = None;
                                                view.form = None;
                                            }
                                            view.sync_component_theme(cx);
                                            for pane in view.panes.values() {
                                                pane.view.update(cx, |terminal, cx| {
                                                    terminal.apply_preferences(
                                                        &view.store.profile.preferences,
                                                        window,
                                                        cx,
                                                    )
                                                });
                                            }
                                            view.focus_terminal(window, cx);
                                            cx.notify();
                                        },
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .id("modal-content")
                                .flex_1()
                                .min_h_0()
                                .when(modal == Modal::Settings, |this| this.overflow_hidden())
                                .when(modal != Modal::Settings, |this| {
                                    this.overflow_y_scroll().p_6()
                                })
                                .child(content),
                        )
                        .children(
                            (modal == Modal::Connection)
                                .then(|| self.render_connection_footer(form, cx)),
                        )
                        .children(
                            (modal == Modal::Settings
                                && matches!(
                                    self.settings_page,
                                    SettingsPage::General
                                        | SettingsPage::Appearance
                                        | SettingsPage::Terminal
                                ))
                            .then(|| {
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .justify_end()
                                    .px_6()
                                    .py_3()
                                    .border_t_1()
                                    .border_color(rgb(p.border))
                                    .child(
                                        crate::controls::button("save-settings")
                                            .primary()
                                            .label(crate::i18n::text("保存设置"))
                                            .disabled(
                                                self.configuration_save.is_some()
                                                    || self.data_busy.is_some(),
                                            )
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.save_settings(window, cx)
                                            })),
                                    )
                            }),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Drop for WorkspaceView {
    fn drop(&mut self) {
        crate::config::clear_secret_cache();
        for job in &self.jobs {
            job.abort();
        }
        self.transfer_queue.stop();
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette();
        let active_layout = self
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == self.active_tab)
            .map(|tab| tab.layout.clone());
        let terminal = active_layout
            .map(|layout| self.render_layout(&layout, cx))
            .unwrap_or_else(|| self.render_empty_workspace());
        div()
            .relative()
            .track_focus(&self.modal_focus)
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .font(crate::fonts::FontPair::ui(&self.store.profile.preferences, cx).latin)
            .text_size(px(self.store.profile.preferences.ui_font_size))
            .on_drag_move(cx.listener(
                |view, event: &gpui_kit::DragMoveEvent<ResizePanel>, window, cx| {
                    view.resize_panel(event, window, cx)
                },
            ))
            .on_action(
                cx.listener(|view, _: &crate::Quit, window, cx| view.request_quit(window, cx)),
            )
            .on_action(cx.listener(|view, _: &crate::About, window, cx| {
                view.show_overlay(Overlay::About, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &crate::KeyboardShortcuts, window, cx| {
                    view.show_overlay(Overlay::Shortcuts, window, cx)
                }),
            )
            .on_action(cx.listener(|view, _: &crate::CommandPalette, window, cx| {
                view.show_overlay(Overlay::Palette, window, cx)
            }))
            .on_action(cx.listener(|view, _: &crate::ClosePane, window, cx| {
                if let Some(id) = view.active_pane {
                    view.close_pane(id, window, cx);
                }
            }))
            .on_action(cx.listener(|view, _: &crate::ToggleSidebar, _, cx| {
                view.sidebar.toggle();
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &crate::ShellMode, window, cx| {
                if let Some(id) = view.active_pane {
                    view.set_pane_mode(id, Mode::Shell, window, cx);
                }
            }))
            .on_action(cx.listener(|view, _: &crate::AgentMode, window, cx| {
                if let Some(id) = view.active_pane {
                    view.set_pane_mode(id, Mode::Agent, window, cx);
                }
            }))
            .on_action(cx.listener(|view, _: &crate::Reconnect, _, cx| {
                if let Some(id) = view.active_pane {
                    view.reconnect_pane(id, cx);
                }
            }))
            .on_action(
                cx.listener(|view, _: &crate::OpenFiles, _, cx| view.switch_tool(Tool::Files, cx)),
            )
            .on_action(cx.listener(|view, _: &crate::OpenForwarding, _, cx| {
                view.switch_tool(Tool::Forwarding, cx)
            }))
            .on_action(cx.listener(|view, _: &crate::SearchTerminal, window, cx| {
                if let Some(id) = view.active_pane {
                    view.set_pane_mode(id, Mode::Shell, window, cx);
                    if let Some(pane) = view.panes.get(&id) {
                        pane.view
                            .update(cx, |terminal, cx| terminal.open_search(window, cx));
                    }
                }
            }))
            .on_action(cx.listener(|view, _: &crate::ClearTerminal, _, cx| {
                if let Some(pane) = view.active_pane.and_then(|id| view.panes.get(&id))
                    && let Err(error) = pane.view.read(cx).terminal.engine.clear_screen()
                {
                    view.status = Some(error.to_string());
                }
            }))
            .on_action(
                cx.listener(|view, _: &crate::NewTerminal, window, cx| {
                    view.local(None, window, cx)
                }),
            )
            .on_action(cx.listener(|view, _: &crate::OpenSettings, window, cx| {
                view.settings_form(window, cx)
            }))
            .on_action(cx.listener(|view, _: &crate::SplitRight, window, cx| {
                view.split(Axis::Horizontal, window, cx)
            }))
            .on_action(cx.listener(|view, _: &crate::SplitDown, window, cx| {
                view.split(Axis::Vertical, window, cx)
            }))
            .on_key_down(
                cx.listener(|view, event: &gpui_kit::KeyDownEvent, window, cx| {
                    if view
                        .auth_dialog
                        .as_ref()
                        .is_some_and(|dialog| !dialog.prompt.answer.is_closed())
                        && event.keystroke.key == "escape"
                    {
                        view.cancel_authentication(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    if view.overlay.is_some()
                        || view.modal.is_some()
                        || view.connection_removal.is_some()
                        || view.file_editor.is_some()
                        || view.file_action.is_some()
                        || !view.prompts.is_empty()
                        || view
                            .auth_dialog
                            .as_ref()
                            .is_some_and(|dialog| !dialog.prompt.answer.is_closed())
                    {
                        return;
                    }
                    if event.keystroke.modifiers.platform
                        || (event.keystroke.modifiers.control && event.keystroke.modifiers.shift)
                    {
                        match event.keystroke.key.as_str() {
                            "t" => view.local(None, window, cx),
                            "w" => {
                                if let Some(id) = view.active_pane {
                                    view.close_pane(id, window, cx);
                                }
                            }
                            "d" => view.split(
                                if event.keystroke.modifiers.shift {
                                    Axis::Vertical
                                } else {
                                    Axis::Horizontal
                                },
                                window,
                                cx,
                            ),
                            "," => view.settings_form(window, cx),
                            _ => return,
                        }
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(panels::measure(
                        self.panel_bounds.clone(),
                        cx.entity().downgrade(),
                        None,
                    ))
                    .children(
                        (self.sidebar.extent(self.sidebar_limit(PanelEdge::Left)) > 0.)
                            .then(|| self.render_sidebar(cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(panels::HEADER_BAR))
                                    .flex_shrink_0()
                                    .border_b_1()
                                    .border_color(rgb(p.border))
                                    .bg(rgb(p.sidebar))
                                    .px_2()
                                    .when(
                                        cfg!(target_os = "macos")
                                            && self
                                                .sidebar
                                                .extent(self.sidebar_limit(PanelEdge::Left))
                                                == 0.,
                                        |bar| bar.pl(px(82.)),
                                    )
                                    .gap_1()
                                    .child(
                                        icon_button(
                                            "toggle-sidebar",
                                            IconName::PanelLeft,
                                            crate::i18n::text("连接导航"),
                                        )
                                        .on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.sidebar.toggle();
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .id("tabs")
                                            .flex()
                                            .items_center()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_x_scroll()
                                            .gap_1()
                                            .children(self.tabs.iter().map(|tab| {
                                                let id = tab.id;
                                                let active = Some(id) == self.active_tab;
                                                div()
                                                    .id(format!("tab-{id}"))
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .px_3()
                                                    .h(px(32.))
                                                    .rounded_md()
                                                    .bg(rgb(if active {
                                                        p.hover
                                                    } else {
                                                        p.sidebar
                                                    }))
                                                    .text_color(rgb(if active {
                                                        p.text
                                                    } else {
                                                        p.muted
                                                    }))
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(
                                                        move |view, _, window, cx| {
                                                            view.active_tab = Some(id);
                                                            view.active_pane = view
                                                                .tabs
                                                                .iter()
                                                                .find(|tab| tab.id == id)
                                                                .map(|tab| tab.active_pane);
                                                            view.focus_terminal(window, cx);
                                                            view.reset_files(cx);
                                                            cx.notify();
                                                        },
                                                    ))
                                                    .child(Icon::new(IconName::Terminal).size_3())
                                                    .child(
                                                        div()
                                                            .text_size(px(12.))
                                                            .child(tab.name.clone()),
                                                    )
                                            })),
                                    )
                                    .child(
                                        icon_button(
                                            "new-tab",
                                            IconName::Plus,
                                            crate::i18n::text("新建终端 · ⌘T"),
                                        )
                                        .on_click(
                                            cx.listener(|view, _, window, cx| {
                                                view.local(None, window, cx)
                                            }),
                                        ),
                                    ),
                            )
                            .child(terminal),
                    )
                    .children((self.tool != Tool::Hidden).then(|| self.render_tool(cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(28.))
                    .relative()
                    .px_4()
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.sidebar))
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(5.)).rounded_full().bg(rgb(p.success)))
                            .child(
                                self.current_node()
                                    .map(|node| format!("SSH · {}", node.connection.host))
                                    .unwrap_or_else(|| crate::i18n::text("本地").into()),
                            )
                            .children(self.broadcast.then(|| {
                                div()
                                    .text_color(rgb(p.accent))
                                    .child(crate::i18n::text("广播输入已开启"))
                            })),
                    )
                    .children(
                        self.active_pane
                            .and_then(|id| self.panes.get(&id))
                            .and_then(|pane| pane.view.read(cx).snapshot.directory.clone())
                            .map(|path| {
                                div()
                                    .id("status-directory")
                                    .absolute()
                                    .left(gpui_kit::relative(0.25))
                                    .w(gpui_kit::relative(0.5))
                                    .h_full()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .overflow_x_scroll()
                                    .text_size(px(12.))
                                    .child(
                                        div()
                                            .h_full()
                                            .flex()
                                            .flex_none()
                                            .items_center()
                                            .justify_center()
                                            .min_w_full()
                                            .whitespace_nowrap()
                                            .child(path),
                                    )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(
                                self.active_pane
                                    .and_then(|id| self.panes.get(&id).map(|pane| (id, pane)))
                                    .and_then(|(id, pane)| {
                                        pane.view
                                            .read(cx)
                                            .snapshot
                                            .transfer
                                            .as_ref()
                                            .map(|t| (id, t))
                                    })
                                    .map(|(id, transfer)| {
                                        let progress = if transfer.cancelling {
                                            crate::i18n::text("取消中…").to_string()
                                        } else {
                                            transfer
                                                .total
                                                .filter(|n| *n > 0)
                                                .map(|n| {
                                                    format!(
                                                        "{:.0}%",
                                                        transfer.bytes as f64 / n as f64 * 100.
                                                    )
                                                })
                                                .unwrap_or_else(|| format_bytes(transfer.bytes))
                                        };
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(div().max_w(px(150.)).text_ellipsis().child(
                                                format!("Zmodem · {} · {progress}", transfer.file),
                                            ))
                                            .child(
                                                icon_button(
                                                    "status-cancel-zmodem",
                                                    IconName::X,
                                                    crate::i18n::text("取消传输"),
                                                )
                                                .on_click(cx.listener(move |view, _, _, cx| {
                                                    if let Some(pane) = view.panes.get(&id) {
                                                        pane.view
                                                            .read(cx)
                                                            .terminal
                                                            .engine
                                                            .cancel_zmodem();
                                                    }
                                                    cx.notify();
                                                })),
                                            )
                                    }),
                            )
                            .child(crate::i18n::localize(format!(
                                "{} 会话   ·   UTF-8",
                                self.panes.len()
                            ))),
                    ),
            )
            .children(self.render_modal(cx))
            .children(self.render_overlay(cx))
            .children(self.status.as_ref().map(|message| {
                div()
                    .absolute()
                    .bottom(px(40.))
                    .left(px(260.))
                    .right(px(24.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(p.surface))
                    .text_size(px(12.))
                    .child(message.clone())
                    .child(
                        icon_button("dismiss-status", IconName::X, crate::i18n::text("关闭消息"))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.status = None;
                                cx.notify();
                            })),
                    )
            }))
            .children(self.render_success_notification(window, cx))
    }
}

fn icon_button(id: impl Into<gpui_kit::ElementId>, icon: IconName, tooltip: &str) -> Button {
    crate::controls::button(id)
        .ghost()
        .compact()
        .w(px(32.))
        .h(px(32.))
        .accessibility_label(tooltip.to_string())
        .child(Icon::new(icon).size_5())
        .tooltip(tooltip.to_string())
}
fn modal_container(p: Palette) -> gpui_kit::Div {
    div()
        .absolute()
        .inset_0()
        .occlude()
        .on_scroll(|_, _, cx| cx.stop_propagation())
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x00000088))
        .text_color(rgb(p.text))
}
fn remote_path(directory: &str, name: &str) -> String {
    format!("{}/{name}", directory.trim_end_matches('/'))
}
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    }
}

fn history_tool(current: Tool, active: Option<Uuid>, requested: Uuid, open: bool) -> Tool {
    if current == Tool::History && active == Some(requested) && open {
        Tool::Hidden
    } else {
        Tool::History
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    #[test]
    fn another_pane_switches_history_and_same_pane_toggles_it() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        assert!(history_tool(Tool::History, Some(first), second, true) == Tool::History);
        assert!(history_tool(Tool::History, Some(second), second, true) == Tool::Hidden);
        assert!(history_tool(Tool::History, Some(second), second, false) == Tool::History);
        assert!(history_tool(Tool::Files, Some(first), first, true) == Tool::History);
    }
}
