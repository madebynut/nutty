//! Command proposals taken from AI replies, and the checks every command sent to a
//! terminal must pass. Whether a proposal may be sent without a click is judged by
//! the model (`nutty_ai::command_review`), not by a list of programs here.

use anyhow::{Result, ensure};

pub const MAX_COMMAND_BYTES: usize = 64 * 1024;

pub fn shell_language(language: &str) -> bool {
    matches!(
        language.trim().to_ascii_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "shell" | "powershell" | "pwsh"
    )
}

pub fn validate(command: &str) -> Result<()> {
    ensure!(!command.trim().is_empty(), "请输入命令");
    ensure!(command.len() <= MAX_COMMAND_BYTES, "命令最多为 64 KiB");
    ensure!(
        !command
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t')),
        "命令包含不可发送的控制字符"
    );
    // Embedding, override and isolate controls make the displayed text differ from what
    // the shell receives (Trojan Source). Joiners and other zero-width marks stay allowed.
    ensure!(
        !command.chars().any(
            |character| matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        ),
        "命令包含双向文本控制字符，显示内容可能与实际命令不同"
    );
    Ok(())
}

/// Only explicitly labelled, closed shell fences become executable proposals.
pub fn proposals(text: &str) -> Vec<String> {
    proposal_blocks(text)
        .into_iter()
        .map(|block| block.command)
        .collect()
}

#[derive(Clone, Debug)]
pub struct ProposalBlock {
    pub language: String,
    pub command: String,
}

pub fn language_supported(
    language: &str,
    environment: &crate::session_environment::Environment,
) -> bool {
    if !shell_language(language) {
        return false;
    }
    if matches!(
        language.trim().to_ascii_lowercase().as_str(),
        "powershell" | "pwsh"
    ) {
        environment.shell_is("powershell") || environment.shell_is("pwsh")
    } else {
        environment.supports_posix_commands()
    }
}

pub fn proposal_blocks(text: &str) -> Vec<ProposalBlock> {
    let mut commands = Vec::new();
    let mut fence: Option<(usize, String)> = None;
    let mut content = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let ticks = trimmed
            .chars()
            .take_while(|character| *character == '`')
            .count();
        if let Some((width, language)) = &fence {
            if ticks >= *width && trimmed[ticks..].trim().is_empty() {
                let command = content.trim().to_string();
                if shell_language(language)
                    && validate(&command).is_ok()
                    && !commands
                        .iter()
                        .any(|block: &ProposalBlock| block.command == command)
                    && commands.len() < 16
                {
                    commands.push(ProposalBlock {
                        command,
                        language: language.clone(),
                    });
                }
                fence = None;
                content.clear();
            } else if content.len() <= MAX_COMMAND_BYTES {
                content.push_str(line);
                content.push('\n');
            }
        } else if ticks >= 3 {
            let language = trimmed[ticks..].trim().to_ascii_lowercase();
            fence = Some((ticks, language));
        }
    }
    commands
}

pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposals_preserve_complex_commands_and_exclude_prose_and_other_languages() {
        let text = "Use this:\n```bash\ncat <<'EOF'\nhello $USER\nEOF\n```\n```python\nprint('hello')\n```\n```sh\npwd\n```\n```sh\npwd\n```\n```sh\nunfinished";
        assert_eq!(proposals(text), ["cat <<'EOF'\nhello $USER\nEOF", "pwd"]);
    }

    #[test]
    fn bidirectional_controls_are_rejected_but_joiners_are_not() {
        for control in ('\u{202A}'..='\u{202E}').chain('\u{2066}'..='\u{2069}') {
            let command = format!("ls {control}-la");
            assert!(validate(&command).is_err(), "U+{:04X}", control as u32);
        }
        let text = "```sh\necho 'safe' \u{202E}'txt.exe'\n```\n```sh\npwd\n```";
        assert_eq!(proposals(text), ["pwd"]);
        for command in [
            "echo 👨\u{200D}👩\u{200D}👧",
            "echo 'می\u{200C}خواهم'",
            "echo a\u{200B}b",
        ] {
            assert!(validate(command).is_ok(), "{command}");
        }
    }
}
