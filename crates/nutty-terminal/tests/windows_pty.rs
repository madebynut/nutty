//! Exercise the real Windows ConPTY and PowerShell integration, without user credentials.
#![cfg(windows)]

use anyhow::{Context as _, Result, ensure};
use nutty_core::config::Preferences;
use nutty_terminal::terminal::{Engine, TermSize, Terminal};
use std::time::{Duration, Instant};

fn wait(engine: &Engine, predicate: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !predicate() {
        ensure!(
            Instant::now() < deadline,
            "ConPTY timeout: {}",
            engine.recent_output()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn powershell_tracks_directories_history_unicode_and_isolates_panes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let child = directory.path().join("中文 directory");
    std::fs::create_dir(&child)?;
    let preferences = Preferences {
        directory: directory.path().into(),
        shell: "powershell.exe".into(),
        ..Default::default()
    };
    let first = Terminal::local(&preferences)?;
    let second = Terminal::local(&preferences)?;
    wait(&first.engine, || first.engine.shell_integration_active())?;
    first.engine.resize(TermSize {
        rows: 30,
        columns: 100,
    })?;
    // Type, let the line echo, then press Enter, as a user does: history records the
    // prompt line as it is shown when Enter is pressed.
    let command = "Set-Location -LiteralPath '.\\中文 directory'";
    first.engine.input(command.as_bytes().to_vec())?;
    wait(&first.engine, || first.engine.recent_output().contains(command))?;
    first.engine.input(b"\r".to_vec())?;
    wait(&first.engine, || {
        first
            .engine
            .snapshot()
            .directory
            .as_deref()
            .is_some_and(|path| path.ends_with("中文 directory"))
    })?;
    ensure!(
        first
            .engine
            .command_history()
            .iter()
            .any(|entry| entry.command.contains("Set-Location")),
        "typed command missing from history"
    );
    wait(&first.engine, || {
        first
            .engine
            .automatic_command_ready(first.engine.snapshot().automation_revision)
    })?;
    first
        .engine
        .send_command("[Console]::WriteLine('NUTTY_' + 'WINDOWS_中文')")?;
    wait(&first.engine, || {
        first.engine.recent_output().contains("NUTTY_WINDOWS_中文")
    })?;
    ensure!(
        !second.engine.recent_output().contains("NUTTY_WINDOWS_中文"),
        "command crossed pane boundary"
    );
    let tracked = first
        .engine
        .snapshot()
        .directory
        .context("tracked directory")?;
    assert_eq!(
        std::fs::canonicalize(tracked)?,
        std::fs::canonicalize(child)?
    );
    first.engine.input(b"exit\r".to_vec())?;
    wait(&first.engine, || first.engine.is_closed())?;
    Ok(())
}
