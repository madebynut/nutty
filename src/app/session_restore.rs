//! Reopens local shells and retains disconnected SSH panes in their original layout.
use super::*;
use crate::workspace::session::{self, Snapshot, Source};

impl WorkspaceView {
    pub(super) fn restore_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.store.profile.preferences.restore_workspace {
            self.local(None, window, cx);
            return;
        }
        let snapshot = match session::load(&self.store.directory) {
            Ok((Some(snapshot), notice)) => {
                self.status = notice;
                snapshot
            }
            Ok((None, _)) => {
                self.local(None, window, cx);
                return;
            }
            Err(error) => {
                self.status = Some(format!("{error:#}"));
                self.local(None, window, cx);
                return;
            }
        };
        let layouts = match snapshot.layouts() {
            Ok(layouts) => layouts,
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        self.sidebar.open = snapshot.sidebar_open;
        self.sidebar.preferred = snapshot.sidebar_width;
        self.tool_size.preferred = snapshot.tool_width;
        self.split_sizes = snapshot
            .splits
            .iter()
            .filter_map(|split| split.sizes.map(|sizes| (split.id, sizes)))
            .collect();
        for saved in &snapshot.panes {
            let mut preferences = self.store.profile.preferences.clone();
            let (terminal, directory) = match &saved.source {
                Source::Local { shell, directory } => {
                    preferences.shell = shell.clone();
                    preferences.directory = directory.clone();
                    if !directory.is_dir() {
                        preferences.directory = self.store.profile.preferences.directory.clone();
                        self.status = Some(
                            crate::i18n::text("上次的本地目录已不存在，已使用启动目录").into(),
                        );
                    }
                    match Terminal::local(&preferences) {
                        Ok(terminal) => (
                            terminal,
                            Some(preferences.directory.to_string_lossy().into_owned()),
                        ),
                        Err(error) => {
                            let (terminal, _) = Terminal::channel(
                                &preferences,
                                crate::i18n::text("本地终端").into(),
                            );
                            terminal.engine.fail(format!("{error:#}"));
                            (terminal, None)
                        }
                    }
                }
                Source::Ssh { connection } => {
                    let name = self
                        .store
                        .profile
                        .connections
                        .iter()
                        .find(|c| c.id == *connection)
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| crate::i18n::text("连接已移除").into());
                    let (terminal, _) = Terminal::channel(&preferences, name);
                    terminal.engine.set_environment(
                        nutty_core::session_environment::Environment::unknown_ssh(),
                    );
                    terminal.engine.finish();
                    (terminal, None)
                }
            };
            self.create_pane(saved.id, terminal, None, saved.source.clone(), window, cx);
            if let Some(pane) = self.panes.get_mut(&saved.id) {
                pane.directory = directory;
                pane.dock.mode = if saved.agent {
                    Mode::Agent
                } else {
                    Mode::Shell
                };
                pane.dock.shell.open = saved.shell_open;
                pane.dock.shell.preferred = saved.shell_height;
                pane.dock.agent.open = saved.agent_open;
                pane.dock.agent.preferred = saved.agent_height;
            }
        }
        self.tabs = snapshot
            .tabs
            .iter()
            .zip(layouts)
            .map(|(tab, layout)| Tab {
                id: tab.id,
                name: tab.name.clone(),
                layout,
                active_pane: tab.active_pane,
            })
            .collect();
        self.active_tab = snapshot.active_tab;
        self.active_pane = self
            .tabs
            .iter()
            .find(|tab| Some(tab.id) == self.active_tab)
            .map(|tab| tab.active_pane);
        // Heal a fallback checkpoint on the next background write; the damaged
        // original has already been retained separately by the loader.
        self.workspace_checkpoint = self.status.is_none().then_some(snapshot);
    }

    pub(super) fn workspace_snapshot(&self) -> Snapshot {
        let mut splits = Vec::new();
        fn visit(
            layout: &Layout,
            splits: &mut Vec<session::Split>,
            sizes: &HashMap<Uuid, [f32; 2]>,
        ) {
            if let Layout::Split {
                id,
                axis,
                first,
                second,
            } = layout
            {
                splits.push(session::Split {
                    id: *id,
                    axis: *axis,
                    first: first.id(),
                    second: second.id(),
                    sizes: sizes.get(id).copied(),
                });
                visit(first, splits, sizes);
                visit(second, splits, sizes);
            }
        }
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                visit(&tab.layout, &mut splits, &self.split_sizes);
                session::Tab {
                    id: tab.id,
                    name: tab.name.clone(),
                    root: tab.layout.id(),
                    active_pane: tab.active_pane,
                }
            })
            .collect();
        // Stable ordering keeps comparisons cheap and suppresses redundant writes.
        let panes = self
            .tabs
            .iter()
            .flat_map(|tab| tab.layout.panes())
            .filter_map(|id| {
                let pane = self.panes.get(&id)?;
                let mut source = pane.source.clone();
                if let Source::Local { directory, .. } = &mut source
                    && let Some(current) = &pane.directory
                    && std::path::Path::new(current).is_absolute()
                {
                    *directory = current.into();
                }
                Some(session::Pane {
                    id,
                    source,
                    agent: pane.dock.mode == Mode::Agent,
                    shell_open: pane.dock.shell.open,
                    agent_open: pane.dock.agent.open,
                    shell_height: pane.dock.shell.preferred,
                    agent_height: pane.dock.agent.preferred,
                })
            })
            .collect();
        Snapshot {
            version: 1,
            tabs,
            panes,
            splits,
            active_tab: self.active_tab,
            sidebar_open: self.sidebar.open,
            sidebar_width: self.sidebar.preferred,
            tool_width: self.tool_size.preferred,
        }
    }

    pub(super) fn checkpoint_workspace(&mut self) {
        let snapshot = self.workspace_snapshot();
        if self.workspace_checkpoint.as_ref() != Some(&snapshot) {
            self.split_sizes
                .retain(|id, _| snapshot.splits.iter().any(|split| split.id == *id));
            self.workspace_writer.submit(snapshot.clone());
            self.workspace_checkpoint = Some(snapshot);
        }
    }

    pub(super) fn start_workspace_checkpoints(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._workspace_timer = Some(cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                if view
                    .update(cx, |view, _| {
                        if !view.shutting_down {
                            view.checkpoint_workspace();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}
