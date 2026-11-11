//! Nutty's native application entry point.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod app;
mod benchmark;
mod branding;
mod controls;
mod fonts;
mod menus;
mod terminal_view;

use nutty_ai::{acp, ai, assistant_session, integrations, provider_catalog};
use nutty_core::{
    agent_discovery, command, config, i18n, network, portable, provider, shells, ssh_options,
    theme, workspace,
};
use nutty_ssh::{remote, sftp_edit, ssh_agent, transfers};
use nutty_terminal::{journal, terminal};

use gpui_kit::{AppContext, WindowBounds, WindowOptions, px, size};

gpui_kit::actions!(
    nutty,
    [
        Quit,
        NewTerminal,
        OpenSettings,
        SplitRight,
        SplitDown,
        About,
        CommandPalette,
        KeyboardShortcuts,
        Hide,
        HideOthers,
        ShowAll,
        ClosePane,
        ToggleSidebar,
        SearchTerminal,
        ClearTerminal,
        ShellMode,
        AgentMode,
        Reconnect,
        OpenFiles,
        OpenForwarding
    ]
);

fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--help") {
        println!(
            "Nutty {}\n\nUsage: Nutty [--benchmark | --version | --help]\n\nWithout arguments, open the native terminal workspace.\nNUTTY_DATA_DIR overrides the configuration directory.",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if arguments.iter().any(|argument| argument == "--version") {
        println!("Nutty {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if arguments.as_slice() == ["--benchmark"] {
        return benchmark::run();
    }
    anyhow::ensure!(
        arguments.is_empty(),
        "unsupported argument; use --help for usage"
    );
    branding::prepare_launch()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let runtime = std::sync::Arc::new(tokio::runtime::Runtime::new()?);
    let settings = config::Store::open(config::data_dir()?)?;
    gpui_kit::application()
        .with_assets(branding::Assets)
        .run(move |cx| {
            branding::register_application();
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(
                if settings.profile.preferences.appearance == config::Appearance::Light {
                    gpui_kit::component::ThemeMode::Light
                } else {
                    gpui_kit::component::ThemeMode::Dark
                },
                None,
                cx,
            );
            cx.on_action(|_: &Hide, cx| cx.hide());
            cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
            cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
            cx.bind_keys([gpui_kit::KeyBinding::new(
                if cfg!(target_os = "macos") {
                    "cmd-q"
                } else {
                    "ctrl-shift-q"
                },
                Quit,
                None,
            )]);
            crate::i18n::set_language(settings.profile.preferences.language);
            menus::install(cx);
            menus::bind_keys(cx);
            terminal_view::bind_keys(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds::centered(
                    None,
                    size(px(1380.), px(860.)),
                    cx,
                ))),
                window_min_size: Some(size(px(900.), px(560.))),
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("Nutty".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            };
            match gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::WorkspaceView::new(settings, runtime, window, cx))
            }) {
                Ok((handle, workspace)) => {
                    // Native menu dispatch also needs a route when no child has focus.
                    macro_rules! route {
                        ($($action:ty),+ $(,)?) => {$({
                            let workspace = workspace.downgrade();
                            cx.on_action(move |action: &$action, cx| {
                                let _ = handle.update(cx, |_, window, cx| {
                                    let _ = workspace.update(cx, |view, cx| {
                                        view.menu_action(action, window, cx);
                                    });
                                });
                            });
                        })+};
                    }
                    route!(
                        Quit,
                        About,
                        CommandPalette,
                        KeyboardShortcuts,
                        NewTerminal,
                        OpenSettings,
                        SplitRight,
                        SplitDown,
                        ClosePane,
                        ToggleSidebar,
                        SearchTerminal,
                        ClearTerminal,
                        ShellMode,
                        AgentMode,
                        Reconnect,
                        OpenFiles,
                        OpenForwarding
                    );
                }
                Err(error) => {
                    tracing::error!(%error, "could not open application window");
                    cx.quit();
                }
            }
            cx.activate(true);
        });
    Ok(())
}
