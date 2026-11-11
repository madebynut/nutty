//! Native menus and application-wide keyboard bindings.

use crate::{
    About, AgentMode, ClearTerminal, ClosePane, CommandPalette, Hide, HideOthers,
    KeyboardShortcuts, NewTerminal, OpenFiles, OpenForwarding, OpenSettings, Quit, Reconnect,
    SearchTerminal, ShellMode, ShowAll, SplitDown, SplitRight, ToggleSidebar,
};
use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::{App, KeyBinding, Menu, MenuItem, SystemMenuType};

pub(crate) fn install(cx: &mut App) {
    cx.set_menus([
        Menu::new("Nutty").items([
            MenuItem::action(crate::i18n::text("关于 Nutty"), About),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("命令面板…"), CommandPalette),
            MenuItem::action(crate::i18n::text("设置…"), OpenSettings),
            MenuItem::action(crate::i18n::text("键盘快捷键"), KeyboardShortcuts),
            MenuItem::separator(),
            MenuItem::os_submenu(crate::i18n::text("服务"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("隐藏 Nutty"), Hide),
            MenuItem::action(crate::i18n::text("隐藏其他应用"), HideOthers),
            MenuItem::action(crate::i18n::text("显示全部"), ShowAll),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("退出 Nutty…"), Quit),
        ]),
        Menu::new(crate::i18n::text("编辑")).items([
            MenuItem::action(crate::i18n::text("撤销"), Undo),
            MenuItem::action(crate::i18n::text("重做"), Redo),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("剪切"), Cut),
            MenuItem::action(crate::i18n::text("复制"), Copy),
            MenuItem::action(crate::i18n::text("粘贴"), Paste),
            MenuItem::action(crate::i18n::text("全选"), SelectAll),
        ]),
        Menu::new(crate::i18n::text("终端")).items([
            MenuItem::action(crate::i18n::text("新建本地终端"), NewTerminal),
            MenuItem::action(crate::i18n::text("左右分屏"), SplitRight),
            MenuItem::action(crate::i18n::text("上下分屏"), SplitDown),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("查找…"), SearchTerminal),
            MenuItem::action(crate::i18n::text("清空屏幕"), ClearTerminal),
            MenuItem::action("Shell", ShellMode),
            MenuItem::action("Agent", AgentMode),
            MenuItem::separator(),
            MenuItem::action(crate::i18n::text("远程文件"), OpenFiles),
            MenuItem::action(crate::i18n::text("端口转发"), OpenForwarding),
            MenuItem::action(crate::i18n::text("重新连接"), Reconnect),
            MenuItem::action(crate::i18n::text("关闭当前窗格"), ClosePane),
        ]),
        Menu::new(crate::i18n::text("视图")).items([MenuItem::action(
            crate::i18n::text("显示 / 隐藏连接导航"),
            ToggleSidebar,
        )]),
        Menu::new(crate::i18n::text("帮助")).items([
            MenuItem::action(crate::i18n::text("键盘快捷键"), KeyboardShortcuts),
            MenuItem::action(crate::i18n::text("关于 Nutty"), About),
        ]),
    ]);
}

pub(crate) fn bind_keys(cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    cx.bind_keys([
        KeyBinding::new(
            if mac { "cmd-k" } else { "ctrl-shift-k" },
            CommandPalette,
            None,
        ),
        KeyBinding::new(
            if mac { "cmd-," } else { "ctrl-shift-," },
            OpenSettings,
            None,
        ),
        KeyBinding::new(
            if mac { "cmd-/" } else { "ctrl-shift-/" },
            KeyboardShortcuts,
            None,
        ),
        KeyBinding::new(
            if mac { "cmd-t" } else { "ctrl-shift-t" },
            NewTerminal,
            None,
        ),
        KeyBinding::new(if mac { "cmd-w" } else { "ctrl-shift-w" }, ClosePane, None),
        KeyBinding::new(if mac { "cmd-d" } else { "ctrl-shift-d" }, SplitRight, None),
        KeyBinding::new(
            if mac {
                "cmd-shift-d"
            } else {
                "ctrl-shift-alt-d"
            },
            SplitDown,
            None,
        ),
        KeyBinding::new(if mac { "cmd-h" } else { "ctrl-shift-h" }, Hide, None),
        KeyBinding::new(
            if mac { "cmd-alt-h" } else { "ctrl-shift-alt-h" },
            HideOthers,
            None,
        ),
    ]);
}
