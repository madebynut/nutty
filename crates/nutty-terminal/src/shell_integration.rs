//! Shell startup hooks preserve user configuration and emit pane-local OSC metadata.

use anyhow::{Context as _, Result};
use portable_pty::CommandBuilder;
use std::{io::Write as _, path::Path};

pub fn configure(command: &mut CommandBuilder, shell: &Path) -> Result<bool> {
    let name = shell
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), "powershell" | "pwsh") {
        command.args(["-NoLogo", "-NoExit", "-Command", POWERSHELL_INIT]);
        return Ok(true);
    }
    if name == "cmd" {
        command.arg("/k");
        command.arg(cmd_init());
        return Ok(true);
    }
    if !matches!(name.as_str(), "zsh" | "bash" | "fish") {
        return Ok(false);
    }
    let root = crate::config::data_dir()?.join("shell-integration");
    std::fs::create_dir_all(&root)?;
    match name.as_str() {
        "zsh" => {
            write_zsh(&root)?;
            let original = std::env::var_os("ZDOTDIR")
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir)
                .context("home directory unavailable")?;
            command.env("NUTTY_ZDOTDIR", original);
            command.env("NUTTY_HOOK_DIR", &root);
            command.env("ZDOTDIR", &root);
            command.arg("-l");
            Ok(true)
        }
        "bash" => {
            let path = root.join("bashrc");
            write_source(&path, BASH_RC)?;
            command.arg("--rcfile");
            command.arg(path);
            command.arg("-i");
            Ok(true)
        }
        "fish" => {
            command.arg("--init-command");
            command.arg(FISH_INIT);
            command.arg("-l");
            Ok(true)
        }
        _ => Ok(false),
    }
}

// Use -Command so the integration works with the default execution policy. User
// profiles run normally; their prompt is preserved and no profile file is edited.
const POWERSHELL_INIT: &str = r#"$global:__NuttyPrompt = $function:prompt
function global:prompt {
    $nuttySucceeded = $?
    $esc = [char]27; $bel = [char]7
    $code = if ($nuttySucceeded) { 0 } else { 1 }
    [Console]::Write("${esc}]133;D;${code}${bel}")
    $location = Get-Location
    if ($location.Provider.Name -eq 'FileSystem' -and $location.Path -notmatch '[\x00-\x1f\x7f]') {
        [Console]::Write("${esc}]1337;CurrentDir=$($location.Path)${bel}")
    }
    [Console]::Write("${esc}]133;A${bel}")
    $text = if ($global:__NuttyPrompt) { & $global:__NuttyPrompt } else { "PS $location> " }
    "$text${esc}]133;B${bel}"
}
"#;

fn write_zsh(root: &Path) -> Result<()> {
    for (file, source) in [
        (".zshenv", ZSH_ENV),
        (".zprofile", ZSH_PROFILE),
        (".zshrc", ZSH_RC),
        (".zlogin", ZSH_LOGIN),
    ] {
        write_source(&root.join(file), source)?;
    }
    Ok(())
}
const ZSH_ENV: &str = r#"ZDOTDIR="$NUTTY_ZDOTDIR"
[[ -r "$ZDOTDIR/.zshenv" ]] && source "$ZDOTDIR/.zshenv"
NUTTY_ZDOTDIR="${ZDOTDIR:-$HOME}"
ZDOTDIR="$NUTTY_HOOK_DIR"
"#;
const ZSH_PROFILE: &str = r#"ZDOTDIR="$NUTTY_ZDOTDIR"
[[ -r "$ZDOTDIR/.zprofile" ]] && source "$ZDOTDIR/.zprofile"
NUTTY_ZDOTDIR="${ZDOTDIR:-$HOME}"
ZDOTDIR="$NUTTY_HOOK_DIR"
"#;
const ZSH_LOGIN: &str = r#"ZDOTDIR="$NUTTY_ZDOTDIR"
[[ -r "$ZDOTDIR/.zlogin" ]] && source "$ZDOTDIR/.zlogin"
"#;
const ZSH_RC: &str = r#"ZDOTDIR="$NUTTY_ZDOTDIR"
[[ -r "$ZDOTDIR/.zshrc" ]] && source "$ZDOTDIR/.zshrc"
NUTTY_ZDOTDIR="${ZDOTDIR:-$HOME}"
ZDOTDIR="$NUTTY_HOOK_DIR"
function __nutty_directory() {
    local LC_ALL=C value="$PWD" encoded='' byte ordinal i
    for (( i=1; i<=${#value}; i++ )); do
        byte="${value[i]}"
        if [[ "$byte" == [a-zA-Z0-9/._~-] ]]; then encoded+="$byte"
        else printf -v ordinal '%d' "'$byte"; printf -v byte '%%%02X' "$((ordinal & 255))"; encoded+="$byte"; fi
    done
    printf '\033]7;file://%s%s\007' "${HOST:-localhost}" "$encoded"
}
function __nutty_precmd() {
    local __nutty_status=$? suffix=$'%{\e]133;B\a%}'
    PROMPT="${PROMPT%"$suffix"}${suffix}"
    printf '\033]133;D;%s\007' "$__nutty_status"
    __nutty_directory
    printf '\033]133;A\007'
}
function __nutty_preexec() { printf '\033]133;C\007'; }
autoload -Uz add-zsh-hook
add-zsh-hook precmd __nutty_precmd
add-zsh-hook preexec __nutty_preexec
"#;
const BASH_RC: &str = r#"if [[ -r "$HOME/.bash_profile" ]]; then source "$HOME/.bash_profile"
elif [[ -r "$HOME/.bash_login" ]]; then source "$HOME/.bash_login"
elif [[ -r "$HOME/.profile" ]]; then source "$HOME/.profile"
elif [[ -r "$HOME/.bashrc" ]]; then source "$HOME/.bashrc"; fi
__nutty_directory() {
    local LC_ALL=C value="$PWD" encoded='' byte ordinal i
    for (( i=0; i<${#value}; i++ )); do
        byte="${value:i:1}"
        if [[ "$byte" == [a-zA-Z0-9/._~-] ]]; then encoded+="$byte"
        else printf -v ordinal '%d' "'$byte"; printf -v byte '%%%02X' "$((ordinal & 255))"; encoded+="$byte"; fi
    done
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-localhost}" "$encoded"
}
__nutty_precmd() {
    local __nutty_status=$? suffix='\[\e]133;B\a\]'
    PS1="${PS1%"$suffix"}${suffix}"
    printf '\033]133;D;%s\007' "$__nutty_status"
    __nutty_directory
    printf '\033]133;A\007'
}
if [[ "${PROMPT_COMMAND[*]}" != *"__nutty_precmd"* ]]; then
    if declare -p PROMPT_COMMAND 2>/dev/null | command grep -q 'declare -a'; then PROMPT_COMMAND+=(__nutty_precmd)
    else PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND
}__nutty_precmd"; fi
fi
"#;
const FISH_INIT: &str = r#"if not functions -q __nutty_original_prompt
    functions -c fish_prompt __nutty_original_prompt
end
function fish_prompt
    printf '\e]133;A\a'
    printf '\e]7;file://localhost%s\a' (string escape --style=url -- $PWD | string replace -a '%2F' '/')
    __nutty_original_prompt
    printf '\e]133;B\a'
end
function __nutty_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end
"#;

// POSIX PS1 expands these command substitutions at each primary prompt. Only the
// current process is changed; no startup files or DEBUG traps are replaced.
const SH_INIT: &str = r#"if [ "${__nutty_sh_hook:-}" != 1 ]; then
    __nutty_sh_hook=1
    PS1='$(printf "\033]1337;CurrentDir=%s\007\033]133;A\007" "$PWD")'"$PS1"'$(printf "\033]133;B\007")'
fi
"#;

fn write_source(path: &Path, source: &str) -> Result<()> {
    if std::fs::read(path).is_ok_and(|bytes| bytes == source.as_bytes()) {
        return Ok(());
    }
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(source.as_bytes())?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Installs hooks in the current SSH shell without writing remote startup files.
pub fn remote_command(shell: &str) -> Option<String> {
    // The native CurrentDir frame avoids an encoding loop in the bootstrap. Keep each
    // physical input line below canonical PTY limits, including while a shell is starting.
    const DIRECTORY: &str = r#"__nutty_directory(){ case "$PWD" in *[[:cntrl:]]*) return;; esac; printf '\033]1337;CurrentDir=%s\007' "$PWD"; }
"#;
    let hooks = match shell {
        "zsh" => ZSH_RC
            .split_once("function __nutty_precmd()")
            .map(|(_, rest)| format!("{DIRECTORY}function __nutty_precmd(){rest}"))?,
        "bash" => BASH_RC
            .split_once("__nutty_precmd()")
            .map(|(_, rest)| format!("{DIRECTORY}__nutty_precmd(){rest}"))?,
        "fish" => FISH_INIT.to_string(),
        "cmd" => {
            return Some(cmd_prompt("$P$G"));
        }
        "sh" | "dash" | "ash" => {
            let bash = remote_command("bash")?;
            let fallback = eval_script(SH_INIT, false);
            return Some(format!(
                "if test -n \"${{BASH_VERSION-}}\"; then {bash}; else {fallback}; fi"
            ));
        }
        _ => return None,
    };
    Some(eval_script(&hooks, shell == "fish"))
}

fn cmd_init() -> String {
    let prompt = std::env::var("PROMPT").unwrap_or_else(|_| "$P$G".into());
    let prompt = if prompt.len() <= 128
        && prompt.chars().all(|character| {
            character.is_ascii()
                && !matches!(
                    character,
                    '%' | '&' | '|' | '<' | '>' | '^' | '"' | '\n' | '\r' | '\0'
                )
        }) {
        prompt
    } else {
        "$P$G".into()
    };
    cmd_prompt(&prompt)
}

fn cmd_prompt(body: &str) -> String {
    format!("prompt $E]133;D;0$E\\$E]1337;CurrentDir=$P$E\\$E]133;A$E\\{body}$E]133;B$E\\")
}

fn eval_script(source: &str, fish: bool) -> String {
    // Keep physical newlines out of the PTY input: shells otherwise echo PS2 for every
    // line. printf's argument is a literal; shell expansion occurs only inside eval.
    let source = source.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    let escaped = source
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\'', "'\"'\"'");
    if fish {
        format!("eval (printf '%b' '{escaped}' | string collect)")
    } else {
        format!("eval \"$(printf '%b' '{escaped}')\"")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{config::Preferences, terminal::Terminal};

    #[test]
    fn remote_directory_frames_do_not_emit_controls_from_file_names() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bad\x07\x1b]133;C\x07");
        std::fs::create_dir(&path)?;
        for shell in ["bash", "zsh"] {
            let program = format!("/bin/{shell}");
            if !Path::new(&program).exists() {
                continue;
            }
            let hooks = remote_command(shell).context("remote hooks")?;
            let output = std::process::Command::new(program)
                .args(["-f", "-c"])
                .arg(format!("{hooks}; __nutty_precmd"))
                .current_dir(&path)
                .output()?;
            anyhow::ensure!(output.status.success(), "{shell} rejected hooks");
            assert!(
                !output.stdout.windows(5).any(|bytes| bytes == b"133;C"),
                "directory injected a command marker in {shell}"
            );
            assert!(
                !output
                    .stdout
                    .windows(16)
                    .any(|bytes| bytes == b"1337;CurrentDir=")
            );
        }
        Ok(())
    }

    #[test]
    fn zsh_startup_preserves_redirected_user_configuration_and_directory_hooks() -> Result<()> {
        if !Path::new("/bin/zsh").exists() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("hooks");
        let user = directory.path().join("user");
        let alternate = directory.path().join("中文 ' directory");
        for path in [&root, &user, &alternate] {
            std::fs::create_dir(path)?;
        }
        write_zsh(&root)?;
        std::fs::write(user.join(".zshenv"), "ZDOTDIR=$ALT_ZDOTDIR\n")?;
        std::fs::write(
            alternate.join(".zshrc"),
            "PROMPT='fixture% '; print NUTTY_USER_CONFIGURATION\n",
        )?;
        let output = std::process::Command::new("/bin/zsh")
            .args(["-lic", "__nutty_precmd; print -Pn \"$PROMPT\""])
            .env("ZDOTDIR", &root)
            .env("NUTTY_HOOK_DIR", &root)
            .env("NUTTY_ZDOTDIR", &user)
            .env("ALT_ZDOTDIR", &alternate)
            .env("HOME", &user)
            .current_dir(&alternate)
            .output()?;
        anyhow::ensure!(output.status.success(), "isolated zsh startup failed");
        let (terminal, _) = Terminal::channel(&Preferences::default(), "hooks".into());
        terminal.engine.feed(&output.stdout);
        assert!(
            terminal
                .engine
                .recent_output()
                .contains("NUTTY_USER_CONFIGURATION")
        );
        let directory = terminal
            .engine
            .snapshot()
            .directory
            .context("OSC directory")?;
        assert_eq!(
            std::fs::canonicalize(directory)?,
            std::fs::canonicalize(&alternate)?
        );
        Ok(())
    }

    #[test]
    fn remote_hooks_load_without_writing_user_files_and_encode_unicode_directories() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("中文 ' directory");
        std::fs::create_dir(&path)?;
        for shell in ["zsh", "bash"] {
            let program = format!("/bin/{shell}");
            if !Path::new(&program).exists() {
                continue;
            }
            let hooks = remote_command(shell).context("remote hooks")?;
            let output = std::process::Command::new(program)
                .args(if shell == "zsh" {
                    vec!["-f", "-c"]
                } else {
                    vec!["--noprofile", "--norc", "-c"]
                })
                .arg(format!("{hooks}; __nutty_precmd"))
                .current_dir(&path)
                .output()?;
            anyhow::ensure!(output.status.success(), "remote {shell} hooks failed");
            let (terminal, _) = Terminal::channel(&Preferences::default(), "remote hooks".into());
            terminal.engine.feed(&output.stdout);
            let directory = terminal.engine.snapshot().directory.with_context(|| {
                format!(
                    "remote OSC directory for {shell}: {:?}",
                    String::from_utf8_lossy(&output.stdout)
                )
            })?;
            assert_eq!(
                std::fs::canonicalize(directory)?,
                std::fs::canonicalize(&path)?,
                "{shell}"
            );
        }
        assert!(
            std::fs::read_dir(&path)?.next().is_none(),
            "remote hook setup creates no files"
        );
        Ok(())
    }
}

#[cfg(test)]
mod cmd_integration {
    use super::remote_command;

    #[test]
    fn remote_cmd_reports_directory_without_evaluating_a_script() {
        let command = remote_command("cmd").expect("cmd hook");
        assert!(command.contains("1337;CurrentDir=$P"));
        assert!(command.starts_with("prompt "));
        assert!(!command.contains("eval"));
    }
}
