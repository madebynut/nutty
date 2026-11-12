//! Literal command proposals and a deliberately narrow automatic execution policy.

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

/// Validates every stage and quotes literal arguments, bypassing aliases and expansion.
/// Only whitelisted read-only programs may appear in pipelines or conditional chains.
pub fn automatic_read_only(command: &str) -> Option<String> {
    prepare_read_only(command).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomaticRejection {
    Syntax,
    TerminalInput(String),
    UnsupportedStage(String),
}
impl std::fmt::Display for AutomaticRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax => f.write_str("包含未支持的展开或脚本结构，请手动执行"),
            Self::TerminalInput(program) => {
                write!(f, "{} 可能等待终端输入，请指定文件或管道", program)
            }
            Self::UnsupportedStage(program) => {
                write!(f, "{} 的命令或参数尚未通过自动只读校验", program)
            }
        }
    }
}
impl std::error::Error for AutomaticRejection {}

/// Compile approved syntax into individually quoted commands, with a useful rejection reason.
pub fn prepare_read_only(command: &str) -> Result<String, AutomaticRejection> {
    validate(command).map_err(|_| AutomaticRejection::Syntax)?;
    let parts = literal_pipeline(command).ok_or(AutomaticRejection::Syntax)?;
    let mut output = Vec::new();
    let mut piped_input = false;
    for (words, operator) in parts {
        let program = words
            .first()
            .and_then(Word::literal)
            .ok_or(AutomaticRejection::Syntax)?;
        let literals: Option<Vec<String>> = words
            .iter()
            .map(|word| word.literal().map(str::to_owned))
            .collect();
        if !piped_input
            && literals
                .as_ref()
                .is_some_and(|words| reads_standard_input(words))
        {
            return Err(AutomaticRejection::TerminalInput(program.into()));
        }
        let compiled = if program == "printf" {
            read_only_printf(&words[1..])
        } else {
            literals.as_deref().and_then(read_only_stage)
        }
        .ok_or_else(|| AutomaticRejection::UnsupportedStage(program.into()))?;
        output.push(compiled);
        piped_input = operator.as_deref() == Some("|");
        if let Some(operator) = operator {
            output.push(operator);
        }
    }
    Ok(output.join(" "))
}

#[derive(Debug)]
enum Word {
    Literal(String),
    ShellName,
}
impl Word {
    fn literal(&self) -> Option<&str> {
        match self {
            Self::Literal(value) => Some(value),
            Self::ShellName => None,
        }
    }
}

fn read_only_printf(arguments: &[Word]) -> Option<String> {
    let (format, values) = arguments.split_first()?;
    let format = format.literal()?;
    if format.starts_with('-') {
        return None;
    }
    let mut chars = format.chars();
    let mut strings = 0;
    while let Some(c) = chars.next() {
        match c {
            '%' => match chars.next()? {
                's' => strings += 1,
                '%' => {}
                _ => return None,
            },
            '\\' if !matches!(chars.next()?, 'n' | 't' | '\\') => return None,
            _ => {}
        }
    }
    if (strings == 0 && !values.is_empty())
        || (strings > 0 && (values.is_empty() || values.len() % strings != 0))
    {
        return None;
    }
    let mut words = vec![quote("/usr/bin/printf"), quote(format)];
    words.extend(values.iter().map(|word| match word {
        Word::Literal(value) => quote(value),
        // This one quoted shell parameter is only allowed as a printf data argument.
        // No parameter operators, splitting, substitution or executable positions.
        Word::ShellName => "\"$0\"".into(),
    }));
    Some(words.join(" "))
}

fn reads_standard_input(tokens: &[String]) -> bool {
    let Some((program, arguments)) = tokens.split_first() else {
        return false;
    };
    if !["cat", "grep", "wc", "head", "tail", "sort"].contains(&program.as_str()) {
        return false;
    }
    let mut arguments = arguments.iter();
    let mut options = true;
    let mut skip_pattern = program == "grep";
    let mut has_file = false;
    while let Some(argument) = arguments.next() {
        if options && argument == "--" {
            options = false;
            continue;
        }
        if options && argument.starts_with('-') && argument != "-" {
            if ["head", "tail"].contains(&program.as_str())
                && ["-n", "-c"].contains(&argument.as_str())
            {
                let _ = arguments.next();
            }
            continue;
        }
        if skip_pattern {
            skip_pattern = false;
            continue;
        }
        if argument == "-" {
            return true;
        }
        has_file = true;
    }
    !has_file
}

fn read_only_stage(tokens: &[String]) -> Option<String> {
    let (program, arguments) = tokens.split_first()?;
    let executable = match program.as_str() {
        "pwd"
            if arguments.is_empty()
                || arguments.len() == 1 && ["-L", "-P"].contains(&arguments[0].as_str()) =>
        {
            "/bin/pwd"
        }
        "uname"
            if arguments.iter().all(|argument| {
                argument == "--all"
                    || argument.strip_prefix('-').is_some_and(|flags| {
                        !flags.is_empty() && flags.chars().all(|flag| "asnrvmpio".contains(flag))
                    })
            }) =>
        {
            "/usr/bin/uname"
        }
        "whoami" if arguments.is_empty() => "/usr/bin/whoami",
        "id" if arguments.iter().all(|argument| {
            ["-u", "-g", "-G", "-n", "-r", "-un", "-gn", "-Gn"].contains(&argument.as_str())
        }) =>
        {
            "/usr/bin/id"
        }
        "uptime" if arguments.is_empty() => "/usr/bin/uptime",
        "date" if arguments.is_empty() => "/bin/date",
        "df" if arguments.is_empty() || arguments == ["-h"] => "/bin/df",
        "ps" if arguments.is_empty()
            || arguments.len() == 1
                && ["-ef", "aux", "-ax", "-e"].contains(&arguments[0].as_str()) =>
        {
            "/bin/ps"
        }
        "ls" if arguments.iter().all(|argument| {
            !argument.starts_with('-')
                || argument == "--"
                || argument.len() > 1
                    && argument[1..]
                        .chars()
                        .all(|character| "alhAdtrS1p".contains(character))
        }) =>
        {
            "/bin/ls"
        }
        "env" if arguments.is_empty() => "/usr/bin/env",
        "echo" if arguments.iter().all(|arg| !arg.starts_with('-')) => "/bin/echo",
        "cat"
            if arguments.iter().all(|arg| {
                !arg.starts_with('-') || ["-n", "-b", "-s", "--"].contains(&arg.as_str())
            }) =>
        {
            "/bin/cat"
        }
        "grep"
            if arguments.iter().all(|arg| {
                !arg.starts_with('-')
                    || arg == "--"
                    || arg.len() > 1 && arg[1..].chars().all(|c| "iEvFxncHhqlws".contains(c))
            }) =>
        {
            "/usr/bin/grep"
        }
        "wc" if arguments.iter().all(|arg| {
            !arg.starts_with('-')
                || arg == "--"
                || arg.len() > 1 && arg[1..].chars().all(|c| "clmwL".contains(c))
        }) =>
        {
            "/usr/bin/wc"
        }
        "head" | "tail" if bounded_lines(arguments) => {
            if program == "head" {
                "/usr/bin/head"
            } else {
                "/usr/bin/tail"
            }
        }
        "sort"
            if arguments.iter().all(|arg| {
                !arg.starts_with('-')
                    || arg == "--"
                    || arg.len() > 1 && arg[1..].chars().all(|c| "nrufb".contains(c))
            }) =>
        {
            "/usr/bin/sort"
        }
        _ => return None,
    };
    Some(
        std::iter::once(executable)
            .chain(arguments.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// PowerShell-only read-only form. Input is parsed by the same literal pipeline as POSIX,
/// then rewritten to module-qualified cmdlets so aliases and functions cannot intercept it.
pub fn prepare_powershell_read_only(command: &str) -> Result<String, AutomaticRejection> {
    validate(command).map_err(|_| AutomaticRejection::Syntax)?;
    let parts = literal_pipeline(command).ok_or(AutomaticRejection::Syntax)?;
    let mut output = Vec::new();
    for (words, operator) in parts {
        if words.iter().any(|word| matches!(word, Word::ShellName)) {
            return Err(AutomaticRejection::Syntax);
        }
        let literals: Option<Vec<String>> = words
            .iter()
            .map(|word| word.literal().map(str::to_owned))
            .collect();
        let literals = literals.ok_or(AutomaticRejection::Syntax)?;
        let program = literals
            .first()
            .ok_or(AutomaticRejection::Syntax)?
            .to_ascii_lowercase();
        let compiled = powershell_stage(&program, &literals[1..])
            .ok_or(AutomaticRejection::UnsupportedStage(program))?;
        output.push(compiled);
        if let Some(operator) = operator {
            if operator != "|" && operator != ";" {
                return Err(AutomaticRejection::Syntax);
            }
            output.push(operator);
        }
    }
    Ok(output.join(" "))
}

fn quote_powershell(word: &str) -> String {
    format!("'{}'", word.replace('\'', "''"))
}

fn emit_powershell(command: &str, arguments: &[String]) -> String {
    let mut output = vec![command.to_owned()];
    let mut expect_number = false;
    for argument in arguments {
        let lower = argument.to_ascii_lowercase();
        if expect_number && powershell_number(argument) {
            output.push(argument.clone());
            expect_number = false;
            continue;
        }
        expect_number = false;
        if [
            "-name",
            "-file",
            "-directory",
            "-force",
            "-hidden",
            "-path",
            "-literalpath",
            "-totalcount",
            "-tail",
        ]
        .contains(&lower.as_str())
        {
            output.push(argument.clone());
            expect_number = matches!(lower.as_str(), "-totalcount" | "-tail");
        } else {
            output.push(quote_powershell(argument));
        }
    }
    output.join(" ")
}

fn powershell_number(value: &str) -> bool {
    value
        .parse::<u32>()
        .is_ok_and(|value| (1..=10_000).contains(&value))
}

fn powershell_stage(program: &str, arguments: &[String]) -> Option<String> {
    if arguments
        .iter()
        .any(|argument| argument.contains([',', ';', '|', '`', '$', '(', ')']))
    {
        return None;
    }
    match program {
        "pwd" | "gl" | "get-location" if arguments.is_empty() => {
            Some("Microsoft.PowerShell.Management\\Get-Location".into())
        }
        "get-date" if arguments.is_empty() => Some("Microsoft.PowerShell.Utility\\Get-Date".into()),
        "whoami" if arguments.is_empty() => Some("& 'C:\\Windows\\System32\\whoami.exe'".into()),
        "hostname" if arguments.is_empty() => {
            Some("& 'C:\\Windows\\System32\\hostname.exe'".into())
        }
        "echo" | "write-output" if arguments.iter().all(|argument| !argument.starts_with('-')) => {
            Some(emit_powershell(
                "Microsoft.PowerShell.Utility\\Write-Output",
                arguments,
            ))
        }
        "ls" | "dir" | "gci" | "get-childitem" if powershell_child(arguments) => Some(
            emit_powershell("Microsoft.PowerShell.Management\\Get-ChildItem", arguments),
        ),
        "cat" | "gc" | "type" | "get-content" => powershell_content(arguments),
        _ => None,
    }
}

fn powershell_child(arguments: &[String]) -> bool {
    let mut words = arguments.iter();
    while let Some(argument) = words.next() {
        let lower = argument.to_ascii_lowercase();
        if ["-literalpath", "-path"].contains(&lower.as_str()) {
            if words.next().is_none_or(|value| value.starts_with('-')) {
                return false;
            }
        } else if argument.starts_with('-')
            && !["-name", "-file", "-directory", "-force", "-hidden"].contains(&lower.as_str())
        {
            return false;
        }
    }
    true
}

fn powershell_content(arguments: &[String]) -> Option<String> {
    let mut words = arguments.iter();
    let mut bounded = false;
    let mut has_path = false;
    while let Some(argument) = words.next() {
        let lower = argument.to_ascii_lowercase();
        if ["-totalcount", "-tail"].contains(&lower.as_str()) {
            if !words.next().is_some_and(|value| powershell_number(value)) {
                return None;
            }
            bounded = true;
        } else if ["-path", "-literalpath"].contains(&lower.as_str()) {
            words.next()?;
            has_path = true;
        } else if argument.starts_with('-') {
            return None;
        } else {
            has_path = true;
        }
    }
    if !has_path {
        return None;
    }
    let mut arguments = arguments.to_vec();
    if !bounded {
        arguments.push("-TotalCount".into());
        arguments.push("200".into());
    }
    Some(emit_powershell(
        "Microsoft.PowerShell.Management\\Get-Content",
        &arguments,
    ))
}

fn bounded_lines(arguments: &[String]) -> bool {
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        if word == "-n" || word == "-c" {
            if !words
                .next()
                .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n > 0 && n <= 100_000))
            {
                return false;
            }
        } else if word.starts_with('-') && word != "--" {
            return false;
        }
    }
    true
}

fn literal_pipeline(command: &str) -> Option<Vec<(Vec<Word>, Option<String>)>> {
    let mut parts = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = None;
    let mut started = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(delimiter) = quoted {
            if matches!(character, '\n' | '\r') {
                return None;
            }
            if character == delimiter {
                quoted = None;
            } else if delimiter == '\'' || !matches!(character, '$' | '`' | '\\') {
                word.push(character);
            } else {
                return None;
            }
        } else if character == '"' && !started {
            let mut next = chars.clone();
            if next.next() == Some('$') && next.next() == Some('0') && next.next() == Some('"') {
                if next
                    .peek()
                    .is_some_and(|c| !c.is_whitespace() && !matches!(c, '|' | '&' | ';'))
                {
                    return None;
                }
                words.push(Word::ShellName);
                chars = next;
            } else {
                quoted = Some('"');
                started = true;
            }
        } else if matches!(character, '\'' | '"') {
            quoted = Some(character);
            started = true;
        } else if matches!(character, '|' | '&' | ';' | '\n' | '\r') {
            if started {
                words.push(Word::Literal(std::mem::take(&mut word)));
                started = false;
            }
            if matches!(character, '\n' | '\r') && words.is_empty() {
                continue;
            }
            if words.is_empty() || words.len() > 32 || parts.len() >= 15 {
                return None;
            }
            let mut operator = if matches!(character, '\n' | '\r') {
                ";".into()
            } else {
                character.to_string()
            };
            if matches!(character, '|' | '&') && chars.peek() == Some(&character) {
                operator.push(chars.next()?);
            }
            if operator == "&" {
                return None;
            }
            parts.push((std::mem::take(&mut words), Some(operator)));
        } else if matches!(
            character,
            '$' | '`' | '\\' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' | '~'
        ) {
            return None;
        } else if character.is_whitespace() {
            if started {
                words.push(Word::Literal(std::mem::take(&mut word)));
                started = false;
            }
        } else {
            word.push(character);
            started = true;
        }
    }
    if quoted.is_some() {
        return None;
    }
    if started {
        words.push(Word::Literal(word));
    }
    if words.is_empty() {
        let (_, operator) = parts.last_mut()?;
        if operator.as_deref() != Some(";") {
            return None;
        }
        *operator = None;
    } else {
        if words.len() > 32 {
            return None;
        }
        parts.push((words, None));
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_diagnostic_is_eligible_for_automatic_execution() {
        assert_eq!(
            automatic_read_only(
                "uname -srm; printf 'shell=%s\\n' \"$0\"; pwd; ls -la | head -n 40"
            ),
            Some("'/usr/bin/uname' '-srm' ; '/usr/bin/printf' 'shell=%s\\n' \"$0\" ; '/bin/pwd' ; '/bin/ls' '-la' | '/usr/bin/head' '-n' '40'".into())
        );
    }

    #[test]
    fn literal_sequences_and_printf_cannot_escape_the_read_only_policy() {
        for command in [
            "uname -srm; pwd; ls -la | head -n 40;",
            "pwd\nls -la\r\nuname -srm\n",
            "printf '%s\\n' \"$0\" 'literal $HOME `id` $(touch x)'",
            "printf '%%\\t%s\\n' 'single '\"'\"'quote'",
            "printf 'done\\n'",
        ] {
            assert!(prepare_read_only(command).is_ok(), "{command}");
        }
        for command in [
            "pwd; rm file",
            "pwd\ntouch file",
            "pwd;; ls",
            "pwd; ; ls",
            "printf '%s\\n' $0",
            "printf '%s\\n' \"${0}\"",
            "printf '%s\\n' \"$0\"suffix",
            "printf '%s\\n' prefix\"$0\"",
            "printf '%s\\n' \"$0\"'suffix'",
            "printf '%s\\n' \"$(touch x)\"",
            "printf '%s\\n' \"$HOME\"",
            "printf '%s\\n' \"$0\" > file",
            "printf '%s\\n' \"$0\" &",
            "printf '%s\\n' \"$0\" | sh",
            "printf \"$0\"",
            "\"$0\"",
            "echo \"$0\"",
            "cat \"$0\"",
            "printf '%b' '\\033]52;c;secret\\a'",
            "printf '%n' name",
            "printf '%999999999s' x",
            "printf '\\033'",
            "printf '--help'",
            "printf '%s %s' x",
            "printf '%s'",
            "printf 'literal' extra",
            "pwd; cat",
            "pwd\ngrep pattern",
            "uname -srmX",
        ] {
            assert!(
                prepare_read_only(command).is_err(),
                "must reject: {command}"
            );
        }
        assert_eq!(
            prepare_read_only("printf '%s' '$(touch x)'"),
            Ok("'/usr/bin/printf' '%s' '$(touch x)'".into())
        );
        assert!(
            matches!(prepare_read_only("pwd; cat"), Err(AutomaticRejection::TerminalInput(program)) if program == "cat")
        );
        assert!(
            matches!(prepare_read_only("pwd; touch x"), Err(AutomaticRejection::UnsupportedStage(program)) if program == "touch")
        );
    }

    #[test]
    fn proposals_preserve_complex_commands_and_exclude_prose_and_other_languages() {
        let text = "Use this:\n```bash\ncat <<'EOF'\nhello $USER\nEOF\n```\n```python\nprint('hello')\n```\n```sh\npwd\n```\n```sh\npwd\n```\n```sh\nunfinished";
        assert_eq!(proposals(text), ["cat <<'EOF'\nhello $USER\nEOF", "pwd"]);
    }

    #[test]
    fn read_only_pipeline_and_conditional_fallback_are_literal_and_safe() {
        assert_eq!(automatic_read_only("env | grep -iE 'model|anthropic|openai|claude|gpt' || echo '未发现相关环境变量'"), Some("'/usr/bin/env' | '/usr/bin/grep' '-iE' 'model|anthropic|openai|claude|gpt' || '/bin/echo' '未发现相关环境变量'".into()));
        assert!(automatic_read_only("pwd && ls -la | head -n 20").is_some());
        for bad in [
            "env sh",
            "ls |",
            "| ls",
            "pwd & ls",
            "pwd || touch x",
            "ls | sort -o file",
            "cat x | tail -f",
            "grep -R x .",
            "echo $(id)",
        ] {
            assert!(automatic_read_only(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn automatic_filters_require_file_operands_or_piped_input() {
        for command in [
            "cat",
            "wc -l",
            "sort -n",
            "head -n 20",
            "tail -c 100",
            "grep -i pattern",
            "cat file -",
            "grep pattern -",
            "echo x && wc -l",
            "echo x | grep x || wc -l",
        ] {
            assert!(
                automatic_read_only(command).is_none(),
                "command must not wait for terminal input: {command}"
            );
        }
        for command in [
            "cat file | sort | wc -l",
            "echo x | cat | sort",
            "echo x | grep x | head -n 20",
            "head -n 20 file",
            "tail -c 100 file",
            "grep -i pattern file",
            "wc -l -- file",
            "echo x | grep x || cat file",
        ] {
            assert!(
                automatic_read_only(command).is_some(),
                "command reads a file or piped input: {command}"
            );
        }
    }

    #[test]
    fn automatic_execution_rejects_mutation_expansion_and_ambiguous_syntax() {
        assert_eq!(
            automatic_read_only("ls -la 'a b'"),
            Some("'/bin/ls' '-la' 'a b'".into())
        );
        assert_eq!(automatic_read_only("pwd"), Some("'/bin/pwd'".into()));
        for command in [
            "rm -rf /",
            "date -s now",
            "pwd; touch x",
            "ls $(touch x)",
            "ls `id`",
            "ls > x",
            "ls | sh",
            "ls\ntouch x",
            "git -c core.fsmonitor=evil status",
            "find . -exec sh {} ;",
            "awk 'BEGIN {system(\"touch x\")}'",
            "sed -i x y",
            "ls --help",
            "ls 'unfinished",
            "ls a\\ b",
            "ls *",
        ] {
            assert!(
                automatic_read_only(command).is_none(),
                "command must require an explicit click: {command}"
            );
        }
    }

    #[test]
    fn powershell_read_only_uses_module_qualified_cmdlets() {
        assert_eq!(
            prepare_powershell_read_only("pwd").expect("location"),
            "Microsoft.PowerShell.Management\\Get-Location"
        );
        assert_eq!(
            prepare_powershell_read_only("Get-Content readme.md").expect("content"),
            "Microsoft.PowerShell.Management\\Get-Content 'readme.md' -TotalCount 200"
        );
        assert_eq!(
            prepare_powershell_read_only("ls -Name docs").expect("listing"),
            "Microsoft.PowerShell.Management\\Get-ChildItem -Name 'docs'"
        );
        assert_eq!(automatic_read_only("pwd"), Some("'/bin/pwd'".into()));
        for command in [
            "Remove-Item readme.md",
            "Get-Content",
            "pwd $(whoami)",
            "ls -Recurse",
            "echo \"$0\"",
        ] {
            assert!(
                prepare_powershell_read_only(command).is_err(),
                "powershell command must stay manual: {command}"
            );
        }
    }
}
