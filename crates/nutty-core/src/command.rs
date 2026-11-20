//! Literal command proposals and a deliberately narrow automatic execution policy.

use anyhow::{Result, ensure};

use crate::session_environment::Platform;

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
    WrongPlatform {
        program: String,
        platform: &'static str,
    },
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
            Self::WrongPlatform { program, platform } => {
                write!(f, "{} 不适用于 {}，已阻止自动发送", program, platform)
            }
        }
    }
}
impl std::error::Error for AutomaticRejection {}

/// Platform-independent form, used when the terminal target is not known.
pub fn prepare_read_only(command: &str) -> Result<String, AutomaticRejection> {
    prepare_read_only_for(command, Platform::Unknown)
}

/// Compile approved syntax into individually quoted commands, with a useful rejection reason.
/// A known target platform also rejects programs and paths that only exist elsewhere.
pub fn prepare_read_only_for(
    command: &str,
    platform: Platform,
) -> Result<String, AutomaticRejection> {
    validate(command).map_err(|_| AutomaticRejection::Syntax)?;
    let parts = literal_pipeline(command).ok_or(AutomaticRejection::Syntax)?;
    let mut output = Vec::new();
    let mut piped_input = false;
    for (mut words, operator) in parts {
        let discard_errors = words.iter().any(|word| matches!(word, Word::DiscardErrors));
        words.retain(|word| !matches!(word, Word::DiscardErrors));
        let program = words
            .first()
            .and_then(Word::literal)
            .ok_or(AutomaticRejection::Syntax)?;
        let mut compiled = if program == "printf" {
            read_only_printf(&words[1..])
                .ok_or_else(|| AutomaticRejection::UnsupportedStage(program.into()))?
        } else {
            let literals: Vec<String> = words
                .iter()
                .map(|word| word.literal().map(str::to_owned))
                .collect::<Option<_>>()
                .ok_or_else(|| AutomaticRejection::UnsupportedStage(program.into()))?;
            read_only_stage(&literals, piped_input, platform)?
        };
        if discard_errors {
            compiled.push_str(" 2>/dev/null");
        }
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
    /// `2>/dev/null`, re-emitted after the stage.
    DiscardErrors,
}
impl Word {
    fn literal(&self) -> Option<&str> {
        match self {
            Self::Literal(value) => Some(value),
            Self::ShellName | Self::DiscardErrors => None,
        }
    }
}

/// Stands for an unquoted leading `~` (alone or before `/`) inside a validated literal,
/// so path checks see it while emission restores the expansion. `validate` rejects
/// control characters, so it never comes from the command text itself.
const HOME: char = '\u{1}';

/// Quotes one argument, leaving a leading home marker unquoted so the shell expands it.
fn quote_argument(word: &str) -> String {
    match word.strip_prefix(HOME) {
        Some("") => "~".into(),
        Some(rest) => format!("~/{}", quote(rest.trim_start_matches('/'))),
        None => quote(word),
    }
}

/// `command` skips aliases and shell functions and resolves the program through the
/// target's own PATH, so one form works across distributions, macOS, BSD and BusyBox.
fn invocation<'a>(program: &str, arguments: impl IntoIterator<Item = &'a str>) -> String {
    std::iter::once("command".to_owned())
        .chain(std::iter::once(quote(program)))
        .chain(arguments.into_iter().map(quote_argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_only_printf(arguments: &[Word]) -> Option<String> {
    let (format, values) = arguments.split_first()?;
    let format = format.literal()?;
    if format.starts_with('-') || format.contains(HOME) {
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
    let mut words = vec![invocation("printf", [format])];
    for word in values {
        words.push(match word {
            Word::Literal(value) => quote_argument(value),
            // This one quoted shell parameter is only allowed as a printf data argument.
            // No parameter operators, splitting, substitution or executable positions.
            Word::ShellName => "\"$0\"".into(),
            Word::DiscardErrors => return None,
        });
    }
    Some(words.join(" "))
}

/// Programs that only exist, or only behave as validated, on some platform families.
fn native_platforms(program: &str) -> Option<&'static [Platform]> {
    use Platform::{Bsd, Linux, MacOs};
    Some(match program {
        "free" | "lscpu" | "lsblk" | "nproc" | "ss" | "ip" | "systemctl" | "journalctl"
        | "dmesg" | "lsb_release" | "hostnamectl" | "timedatectl" | "getenforce" | "sestatus" => {
            &[Linux]
        }
        // BSD vmstat reads `-w`/`-n` values and intervals differently.
        "vmstat" => &[Linux],
        "getent" => &[Linux, Bsd],
        "sw_vers" | "vm_stat" | "launchctl" | "diskutil" | "log" | "scutil" | "system_profiler" => {
            &[MacOs]
        }
        "top" => &[Linux, MacOs],
        _ => return None,
    })
}

/// Short and long options one program accepts; anything else stays manual.
struct Options {
    /// Boolean short options, combinable as in `-la`.
    flags: &'static str,
    /// Short options taking a value, attached (`-n5`) or as the next word.
    valued: &'static str,
    /// Long options without `--`; a trailing `=` means the option takes a value.
    long: &'static [&'static str],
}

#[derive(Default)]
struct Parsed {
    flags: String,
    long: Vec<String>,
    values: Vec<(String, String)>,
    operands: Vec<String>,
}

impl Parsed {
    /// Single letters name short options; longer names are long options without dashes.
    fn has(&self, names: &[&str]) -> bool {
        names.iter().any(|name| {
            let mut letters = name.chars();
            match (letters.next(), letters.next()) {
                (Some(short), None) => self.flags.contains(short),
                _ => self.long.iter().any(|long| long == name),
            }
        })
    }
    fn value(&self, names: &[&str]) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(name, _)| names.contains(&name.as_str()))
            .map(|(_, value)| value.as_str())
    }
}

impl Options {
    const fn new(flags: &'static str, valued: &'static str, long: &'static [&'static str]) -> Self {
        Self {
            flags,
            valued,
            long,
        }
    }
    fn parse(&self, arguments: &[String]) -> Option<Parsed> {
        let mut parsed = Parsed::default();
        let mut words = arguments.iter();
        let mut options = true;
        while let Some(word) = words.next() {
            if options && word == "--" {
                options = false;
            } else if options && let Some(long) = word.strip_prefix("--") {
                let (name, attached) = long.split_once('=').unzip();
                let name = name.unwrap_or(long);
                if attached.is_none() && self.long.contains(&name) {
                    parsed.long.push(name.into());
                } else if self
                    .long
                    .iter()
                    .any(|known| known.strip_suffix('=') == Some(name))
                {
                    let value = match attached {
                        Some(value) => value.to_owned(),
                        None => words.next()?.clone(),
                    };
                    parsed.values.push((name.into(), value));
                } else {
                    return None;
                }
            } else if options && word.len() > 1 && word.starts_with('-') {
                for (index, flag) in word.char_indices().skip(1) {
                    if self.flags.contains(flag) {
                        parsed.flags.push(flag);
                    } else if self.valued.contains(flag) {
                        let rest = &word[index + flag.len_utf8()..];
                        let value = if rest.is_empty() {
                            words.next()?.clone()
                        } else {
                            rest.to_owned()
                        };
                        parsed.values.push((flag.into(), value));
                        break;
                    } else {
                        return None;
                    }
                }
            } else {
                parsed.operands.push(word.clone());
            }
        }
        Some(parsed)
    }
}

/// `log show --last` durations up to one day, such as `30m` or `2h`.
fn bounded_duration(value: &str) -> bool {
    let Some(unit) = value.chars().last() else {
        return false;
    };
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return false,
    };
    number_within(Some(&value[..value.len() - 1]), 86_400 / seconds)
}

/// Plain digits only: `parse` would also accept `+N`, which means "from line N" to tail.
fn number_within(value: Option<&str>, maximum: u32) -> bool {
    value.is_some_and(|value| {
        !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value
                .parse::<u32>()
                .is_ok_and(|value| (1..=maximum).contains(&value))
    })
}

/// `-ft` and `-Aw` combine short options; a letter inside such a word still counts.
fn short_option(word: &str, letter: char) -> bool {
    word.len() > 1
        && word.starts_with('-')
        && !word.starts_with("--")
        && word[1..].chars().all(|c| c.is_ascii_alphanumeric())
        && word[1..].contains(letter)
}

/// Lexical form of a path, so `//dev/tty` or `/./dev/stdin` cannot pass as a plain file.
fn normalized_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if path.starts_with('/') => {}
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if path.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Paths that block on a terminal or kernel stream, or expose process environments.
fn unsafe_path(path: &str) -> bool {
    let path = normalized_path(path);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if !path.starts_with('/') {
        // A relative path that climbs could still reach a device.
        return segments.first() == Some(&"..")
            && segments
                .iter()
                .any(|part| ["dev", "proc", "fd", "sys"].contains(part));
    }
    match segments.as_slice() {
        ["dev", "null"] => false,
        ["dev", ..] => true,
        ["proc", "kmsg" | "kcore" | "sysrq-trigger"] => true,
        ["proc", _, rest @ ..] => rest
            .first()
            .is_some_and(|name| ["fd", "environ", "mem", "fdinfo"].contains(name)),
        ["sys", "kernel", "debug" | "tracing", ..] => true,
        _ => segments.last().is_some_and(|name| *name == "trace_pipe"),
    }
}

/// Paths into common credential stores, whose contents must not reach a model without
/// the user running the command. Lexical and case-insensitive; `\` also separates, for
/// Windows paths.
fn secret_path(path: &str) -> bool {
    let path = normalized_path(&path.replace('\\', "/")).to_lowercase();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    // System account files only count as the file itself, so a project directory
    // named `shadow` stays readable.
    const SYSTEM: &[&str] = &[
        "shadow",
        "shadow-",
        "gshadow",
        "gshadow-",
        "master.passwd",
        "sudoers",
    ];
    parts
        .windows(2)
        .any(|pair| pair == [".docker", "config.json"])
        || parts.last().is_some_and(|name| SYSTEM.contains(name))
        || parts.iter().any(|part| secret_component(part))
}

fn secret_component(name: &str) -> bool {
    const STORES: &[&str] = &[
        ".ssh",
        ".aws",
        ".gnupg",
        ".kube",
        ".netrc",
        "_netrc",
        ".pgpass",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        ".vault-token",
        ".envrc",
        "sudoers.d",
    ];
    const KEYS: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];
    STORES.contains(&name)
        || name == ".env"
        || name.starts_with(".env.")
        // `credentials`, `app-credentials.json`, `.git-credentials`, but not
        // `credentials-guide.md`.
        || name.split('.').any(|part| part.ends_with("credentials"))
        || KEYS.iter().any(|key| name.starts_with(key)) && !name.ends_with(".pub")
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|extension| name.ends_with(extension))
}

/// Variables whose names suggest credentials stay out of automatic output.
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "AUTH",
        "SESSION",
        "COOKIE",
    ]
    .iter()
    .any(|marker| name.contains(marker))
        || name.ends_with("_KEY")
}

/// File operands must be named and finite; `-` and devices would read the terminal.
fn file_operands(
    program: &str,
    files: &[String],
    piped_input: bool,
) -> Result<(), AutomaticRejection> {
    if files.is_empty() && !piped_input || files.iter().any(|file| file == "-") {
        return Err(AutomaticRejection::TerminalInput(program.into()));
    }
    if files.iter().any(|file| unsafe_path(file)) {
        return Err(AutomaticRejection::UnsupportedStage(program.into()));
    }
    Ok(())
}

fn read_only_stage(
    tokens: &[String],
    piped_input: bool,
    platform: Platform,
) -> Result<String, AutomaticRejection> {
    let (program, arguments) = tokens.split_first().ok_or(AutomaticRejection::Syntax)?;
    if let Some(platforms) = native_platforms(program)
        && platform != Platform::Unknown
        && !platforms.contains(&platform)
    {
        return Err(AutomaticRejection::WrongPlatform {
            program: program.clone(),
            platform: platform.label(),
        });
    }
    // Linux pseudo-filesystems are a common wrong guess on macOS.
    if platform == Platform::MacOs
        && let Some(path) = arguments.iter().find(|argument| {
            ["/proc", "/sys"]
                .iter()
                .any(|root| *argument == root || argument.starts_with(&format!("{root}/")))
        })
    {
        return Err(AutomaticRejection::WrongPlatform {
            program: path.clone(),
            platform: platform.label(),
        });
    }
    let extra = stage_arguments(program, arguments, piped_input, platform)?
        .ok_or_else(|| AutomaticRejection::UnsupportedStage(program.clone()))?;
    let mut words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    words.extend_from_slice(extra);
    Ok(invocation(program, words))
}

/// Returns options to append (pagers off), `None` for an unapproved form.
fn stage_arguments(
    program: &str,
    arguments: &[String],
    piped_input: bool,
    platform: Platform,
) -> Result<Option<&'static [&'static str]>, AutomaticRejection> {
    const NONE: &[&str] = &[];
    let parse = |flags: &'static str, valued: &'static str, long: &'static [&'static str]| {
        Options::new(flags, valued, long).parse(arguments)
    };
    let files = |files: &[String]| file_operands(program, files, piped_input);
    // Programs that print file contents must also stay out of credential stores.
    let contents = |operands: &[String]| -> Result<(), AutomaticRejection> {
        files(operands)?;
        if operands.iter().any(|file| secret_path(file)) {
            return Err(AutomaticRejection::UnsupportedStage(program.into()));
        }
        Ok(())
    };
    let accepted = match program {
        "pwd" => parse("LP", "", &[]).is_some_and(|p| p.operands.is_empty()),
        "uname" => parse(
            "asnrvmpio",
            "",
            &[
                "all",
                "kernel-name",
                "kernel-release",
                "machine",
                "nodename",
            ],
        )
        .is_some_and(|p| p.operands.is_empty()),
        "whoami" | "env" | "mount" | "vm_stat" | "getenforce" => arguments.is_empty(),
        "nproc" => parse("", "", &["all"]).is_some_and(|p| p.operands.is_empty()),
        "id" => parse("ugGnr", "", &[]).is_some_and(|p| p.operands.len() <= 1),
        "groups" => parse("", "", &[]).is_some_and(|p| p.operands.len() <= 1),
        // Any operand would rename the host.
        "hostname" => parse("fsidIAa", "", &["fqdn", "short", "all-ip-addresses"])
            .is_some_and(|p| p.operands.is_empty()),
        "uptime" => parse("ps", "", &["pretty", "since"]).is_some_and(|p| p.operands.is_empty()),
        // A non-format operand sets the clock.
        "date" => parse(
            "uRj",
            "dvr",
            &[
                "utc",
                "universal",
                "date=",
                "iso-8601",
                "rfc-email",
                "rfc-3339=",
            ],
        )
        .is_some_and(|p| p.operands.len() <= 1 && p.operands.iter().all(|o| o.starts_with('+'))),
        "df" => parse(
            "ahHiklPT",
            "tx",
            &[
                "human-readable",
                "inodes",
                "local",
                "print-type",
                "total",
                "type=",
                "output=",
            ],
        )
        .is_some(),
        "du" => parse(
            "shcakmxLHP",
            "d",
            &[
                "summarize",
                "human-readable",
                "total",
                "apparent-size",
                "one-file-system",
                "max-depth=",
            ],
        )
        .is_some_and(|p| {
            let depth = p.value(&["d", "max-depth"]);
            p.has(&["s", "summarize"]) && depth.is_none() || number_within(depth, 10)
        }),
        "free" => parse(
            "bkmghltvw",
            "",
            &[
                "bytes", "kilo", "mega", "giga", "human", "si", "total", "wide",
            ],
        )
        .is_some_and(|p| p.operands.is_empty()),
        // ps never writes, but BSD-style `e` (Linux, macOS) and `-E` (macOS) print every
        // process's environment, which commonly holds credentials.
        "ps" => {
            // A word after `-o`, `-eo`, `axo` or `--sort` is a value such as `user`.
            let takes_value = |word: &str| {
                word == "--sort"
                    || !word.starts_with("--")
                        && word
                            .trim_start_matches('-')
                            .chars()
                            .all(|c| c.is_ascii_alphabetic())
                        && word.ends_with(['o', 'O', 'p', 'u', 'U', 'C', 'G', 'g', 't', 'q'])
            };
            arguments.iter().enumerate().all(|(index, word)| {
                let value = index > 0 && takes_value(&arguments[index - 1]);
                if value {
                    true
                } else if let Some(flags) = word.strip_prefix('-') {
                    flags.starts_with('-') || !flags.contains('E')
                } else {
                    !(word.chars().all(|c| c.is_ascii_alphabetic()) && word.contains('e'))
                }
            })
        }
        "stat" | "lscpu" | "lsblk" => true,
        "ls" => parse(
            "1aAbBcCdeEfFgGhHiklLmnoOpqrsStTuUvwxX@",
            "",
            &[
                "all",
                "almost-all",
                "human-readable",
                "color",
                "color=",
                "classify",
                "directory",
                "inode",
                "size",
                "sort=",
                "time-style=",
                "full-time",
                "reverse",
                "group-directories-first",
                "numeric-uid-gid",
                "dereference",
            ],
        )
        .is_some(),
        "printenv" => arguments.iter().all(|name| {
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !secret_name(name)
        }),
        // dash's builtin echo, which `command` selects, interprets backslash escapes and
        // could emit terminal control sequences such as OSC 52.
        "echo" => arguments
            .iter()
            .all(|argument| !argument.starts_with('-') && !argument.contains('\\')),
        "cat" => match parse(
            "nbsvetAET",
            "",
            &["number", "number-nonblank", "squeeze-blank", "show-all"],
        ) {
            Some(p) => {
                contents(&p.operands)?;
                true
            }
            None => false,
        },
        "grep" => match parse(
            "iEFGvxncHhqlLwsoPa",
            "ABCme",
            &[
                "ignore-case",
                "extended-regexp",
                "fixed-strings",
                "invert-match",
                "word-regexp",
                "line-regexp",
                "count",
                "line-number",
                "with-filename",
                "no-filename",
                "files-with-matches",
                "only-matching",
                "quiet",
                "no-messages",
                "text",
                "color",
                "color=",
                "colour=",
                "max-count=",
                "after-context=",
                "before-context=",
                "context=",
                "regexp=",
            ],
        ) {
            Some(p) => {
                let pattern_given = p.value(&["e", "regexp"]).is_some();
                match p.operands.split_first() {
                    _ if pattern_given => contents(&p.operands)?,
                    Some((_, rest)) => contents(rest)?,
                    None => return Err(AutomaticRejection::TerminalInput(program.into())),
                }
                true
            }
            None => false,
        },
        "wc" => match parse("clmwL", "", &["bytes", "chars", "lines", "words"]) {
            Some(p) => {
                files(&p.operands)?;
                true
            }
            None => false,
        },
        // Following, reverse offsets (+N) and unbounded counts stay manual.
        "head" | "tail" => {
            let counted: Vec<String> = arguments
                .iter()
                .map(|word| match word.strip_prefix('-') {
                    Some(count)
                        if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) =>
                    {
                        format!("-n{count}")
                    }
                    _ => word.clone(),
                })
                .collect();
            match Options::new("qv", "nc", &["lines=", "bytes=", "quiet", "verbose"])
                .parse(&counted)
            {
                // `tail +1f` is the historical follow form; `+N` reads from line N.
                Some(p) if p.operands.iter().any(|operand| operand.starts_with('+')) => false,
                Some(p) => {
                    if !p
                        .values
                        .iter()
                        .all(|(_, value)| number_within(Some(value.as_str()), 100_000))
                    {
                        return Ok(None);
                    }
                    contents(&p.operands)?;
                    true
                }
                None => false,
            }
        }
        // -o writes a file; it is deliberately absent.
        "sort" => match parse(
            "bdfghiMnRrsuVcz",
            "kt",
            &[
                "numeric-sort",
                "human-numeric-sort",
                "reverse",
                "unique",
                "ignore-case",
                "key=",
                "field-separator=",
            ],
        ) {
            Some(p) => {
                contents(&p.operands)?;
                true
            }
            None => false,
        },
        // A second operand is an output file.
        "uniq" => match parse(
            "cdiu",
            "fs",
            &["count", "repeated", "ignore-case", "unique"],
        ) {
            Some(p) if p.operands.len() <= 1 => {
                contents(&p.operands)?;
                true
            }
            _ => false,
        },
        "cut" => match parse(
            "sn",
            "bcdf",
            &[
                "bytes=",
                "characters=",
                "delimiter=",
                "fields=",
                "only-delimited",
            ],
        ) {
            Some(p)
                if p.value(&["b", "c", "f", "bytes", "characters", "fields"])
                    .is_some() =>
            {
                contents(&p.operands)?;
                true
            }
            _ => false,
        },
        "tr" => match parse("cdsC", "", &["delete", "squeeze-repeats", "complement"]) {
            Some(p) if (1..=2).contains(&p.operands.len()) => {
                if !piped_input {
                    return Err(AutomaticRejection::TerminalInput(program.into()));
                }
                true
            }
            _ => false,
        },
        "which" => parse("as", "", &[]).is_some_and(|p| !p.operands.is_empty()),
        "w" => parse("hsuif", "", &["no-header", "short", "from"])
            .is_some_and(|p| p.operands.len() <= 1),
        "who" => parse(
            "abdHlmpqrsTu",
            "",
            &["all", "boot", "heading", "count", "runlevel"],
        )
        // Other operands name a utmp file, and a device there would read the terminal.
        .is_some_and(|p| p.operands.is_empty() || p.operands == ["am", "i"]),
        "last" => {
            let counted: Vec<String> = arguments
                .iter()
                .map(|word| match word.strip_prefix('-') {
                    Some(count)
                        if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) =>
                    {
                        format!("-n{count}")
                    }
                    _ => word.clone(),
                })
                .collect();
            Options::new("aiwFx", "n", &[])
                .parse(&counted)
                .is_some_and(|p| number_within(p.value(&["n"]), 1_000) && p.operands.len() <= 2)
        }
        "getconf" => parse("a", "", &[]).is_some_and(|p| {
            p.operands.len() <= 2
                && p.operands.iter().all(|name| {
                    name.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_/".contains(c))
                })
        }),
        // -C compiles a magic database into the working directory.
        "file" => match parse(
            "bLhiIkz",
            "",
            &["brief", "mime", "mime-type", "dereference"],
        ) {
            Some(p) if !p.operands.is_empty() => {
                files(&p.operands)?;
                true
            }
            _ => false,
        },
        // Repeat mode never exits.
        "lsof" => arguments.iter().all(|word| {
            !(word.starts_with("-r") || word.starts_with("+r") || short_option(word, 'r'))
        }),
        // -c (Linux) and -w (BSD) repeat forever, as does a numeric interval.
        // Long options are matched exactly: getopt_long would expand `--cont` to --continuous.
        "netstat" => arguments.iter().all(|word| {
            if let Some(long) = word.strip_prefix("--") {
                [
                    "numeric",
                    "tcp",
                    "udp",
                    "listening",
                    "programs",
                    "all",
                    "route",
                    "interfaces",
                    "statistics",
                    "wide",
                    "extend",
                    "verbose",
                ]
                .contains(&long)
            } else if let Some(flags) = word.strip_prefix('-') {
                !flags.is_empty() && !flags.contains(['c', 'w'])
            } else {
                !word.chars().all(|c| c.is_ascii_digit())
            }
        }),
        // -K closes sockets, -D writes a dump, -E streams events.
        "ss" => parse(
            "tulnpaeoimsxwH460Orb",
            "fA",
            &[
                "tcp",
                "udp",
                "unix",
                "raw",
                "listening",
                "numeric",
                "processes",
                "all",
                "summary",
                "info",
                "extended",
                "memory",
                "options",
                "no-header",
                "ipv4",
                "ipv6",
                "family=",
                "query=",
            ],
        )
        .is_some(),
        "ip" => ip_read_only(arguments),
        "ifconfig" => match parse("alsv", "", &[]) {
            Some(p) if p.flags.is_empty() => {
                p.operands.len() <= 1
                    && p.operands.iter().all(|name| {
                        name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                            && name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
                            && !["up", "down", "add", "delete", "alias", "destroy", "create"]
                                .contains(&name.as_str())
                    })
            }
            Some(p) => p.operands.is_empty(),
            None => false,
        },
        "route" => parse("nv", "", &[]).is_some_and(|p| {
            p.operands.is_empty() || p.operands.len() == 2 && p.operands[0] == "get"
        }),
        "arp" => parse("anv", "", &[]).is_some_and(|p| p.operands.len() <= 1),
        // Without -l, crontab replaces the table from standard input.
        "crontab" => parse("l", "", &[]).is_some_and(|p| p.has(&["l"]) && p.operands.is_empty()),
        "vmstat" => parse(
            if platform == Platform::Linux {
                "adDfmnstw"
            } else {
                "adDfmst"
            },
            "S",
            &["active", "disk", "stats", "timestamp", "wide", "unit="],
        )
        .is_some_and(|p| match p.operands.as_slice() {
            [] => true,
            [delay, count] => {
                number_within(Some(delay.as_str()), 10) && number_within(Some(count.as_str()), 30)
            }
            _ => false,
        }),
        "systemctl" => return Ok(systemctl_read_only(arguments).then_some(no_pager(arguments))),
        "journalctl" => return Ok(journalctl_read_only(arguments).then_some(no_pager(arguments))),
        // -c/-C clear the ring buffer, -n/-D/-E change the console, -w follows, -H pages.
        "dmesg" => parse(
            "TtkuxdrP",
            "lf",
            &[
                "ctime",
                "kernel",
                "userspace",
                "decode",
                "nopager",
                "notime",
                "raw",
                "level=",
                "facility=",
                "time-format=",
            ],
        )
        .is_some_and(|p| p.operands.is_empty()),
        "top" => top_read_only(arguments, platform),
        "sw_vers" => {
            arguments.len() <= 1
                && arguments.iter().all(|word| {
                    [
                        "productName",
                        "productVersion",
                        "productVersionExtra",
                        "buildVersion",
                    ]
                    .contains(&word.trim_start_matches('-'))
                        && word.starts_with('-')
                })
        }
        // -w, -p, -f and name=value forms write kernel parameters.
        "sysctl" => parse(
            "anNbehoxdq",
            "r",
            &["all", "values", "names", "binary", "ignore", "pattern="],
        )
        .is_some_and(|p| {
            p.operands.iter().all(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            })
        }),
        "launchctl" => matches!(
            arguments.first().map(String::as_str),
            Some(
                "list"
                    | "print"
                    | "print-disabled"
                    | "blame"
                    | "version"
                    | "managername"
                    | "manageruid"
                    | "managerpid"
            )
        ),
        "diskutil" => matches!(
            arguments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice(),
            ["list"] | ["list", _] | ["info", _] | ["apfs", "list"] | ["apfs", "list", _]
        ),
        // `log stream` never exits; `log show` must be bounded by --last.
        "log" => {
            arguments.first().is_some_and(|word| word == "show")
                && Options::new(
                    "",
                    "",
                    &[
                        "last=",
                        "predicate=",
                        "style=",
                        "start=",
                        "end=",
                        "info",
                        "debug",
                    ],
                )
                .parse(&arguments[1..])
                .is_some_and(|p| {
                    p.operands.is_empty() && p.value(&["last"]).is_some_and(bounded_duration)
                })
        }
        "scutil" => matches!(
            arguments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice(),
            ["--dns"]
                | ["--proxy"]
                | ["--nwi"]
                | ["--get", "ComputerName" | "LocalHostName" | "HostName"]
        ),
        "system_profiler" => {
            let mut words = arguments.iter();
            let mut types = 0;
            let mut valid = true;
            while let Some(word) = words.next() {
                match word.as_str() {
                    "-detailLevel" => {
                        valid &= words
                            .next()
                            .is_some_and(|level| ["mini", "basic"].contains(&level.as_str()))
                    }
                    "-json" => {}
                    data if data.ends_with("DataType")
                        && data.chars().all(|c| c.is_ascii_alphanumeric()) =>
                    {
                        types += 1
                    }
                    _ => valid = false,
                }
            }
            valid && types > 0
        }
        "docker" | "podman" | "nerdctl" => container_read_only(arguments),
        "kubectl" | "oc" => kubernetes_read_only(arguments),
        // `-f` reads a batch of queries from a file, `-f -` from the terminal.
        "dig" | "host" => {
            arguments.iter().any(|word| !word.starts_with(['-', '+']))
                && !arguments.iter().any(|word| word.starts_with("-f"))
        }
        // Without a name, or with `-`, nslookup becomes interactive.
        "nslookup" => {
            arguments.iter().any(|word| !word.starts_with('-'))
                && arguments.iter().all(|word| word != "-")
        }
        // Flooding, broadcast and custom intervals or sizes stay manual.
        "ping" | "ping6" => parse("nq46vD", "cWwt", &[]).is_some_and(|p| {
            p.operands.len() == 1
                && number_within(p.value(&["c"]), 5)
                && p.values
                    .iter()
                    .all(|(_, value)| number_within(Some(value.as_str()), 10))
        }),
        // shadow and gshadow databases hold password hashes.
        "getent" => {
            matches!(
                arguments.first().map(String::as_str),
                Some(
                    "passwd"
                        | "group"
                        | "hosts"
                        | "ahosts"
                        | "ahostsv4"
                        | "ahostsv6"
                        | "services"
                        | "protocols"
                        | "networks"
                )
            ) && arguments.len() <= 5
        }
        "lsb_release" => parse(
            "aidrcs",
            "",
            &["all", "id", "description", "release", "codename", "short"],
        )
        .is_some_and(|p| p.operands.is_empty()),
        "hostnamectl" | "timedatectl" => {
            matches!(
                arguments
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                [] | ["status"] | ["show"]
            )
        }
        "sestatus" => parse("vb", "", &[]).is_some_and(|p| p.operands.is_empty()),
        _ => false,
    };
    Ok(accepted.then_some(NONE))
}

fn no_pager(arguments: &[String]) -> &'static [&'static str] {
    if arguments.iter().any(|word| word == "--no-pager") {
        &[]
    } else {
        &["--no-pager"]
    }
}

/// Only show/list forms of iproute2 objects; any verb that changes state stays manual.
fn ip_read_only(arguments: &[String]) -> bool {
    const OPTIONS: &[&str] = &[
        "-br", "-brief", "-c", "-color", "-4", "-6", "-s", "-stats", "-d", "-details", "-j",
        "-json", "-p", "-pretty", "-o", "-oneline", "-h", "-human",
    ];
    const OBJECTS: &[&str] = &[
        "a",
        "addr",
        "address",
        "l",
        "link",
        "r",
        "ro",
        "route",
        "n",
        "neigh",
        "neighbor",
        "neighbour",
        "ru",
        "rule",
        "maddr",
        "netconf",
    ];
    const VERBS: &[&str] = &["show", "list", "ls", "lst", "sh", "get"];
    let mut words = arguments
        .iter()
        .map(String::as_str)
        .skip_while(|word| OPTIONS.contains(word));
    if !words.next().is_some_and(|object| OBJECTS.contains(&object)) {
        return false;
    }
    let rest: Vec<_> = words.collect();
    rest.first().is_none_or(|verb| VERBS.contains(verb))
        && rest.iter().skip(1).all(|word| {
            !word.starts_with('-')
                && ![
                    "add", "del", "delete", "set", "change", "replace", "append", "prepend",
                    "flush", "save", "restore", "monitor",
                ]
                .contains(word)
        })
}

fn systemctl_read_only(arguments: &[String]) -> bool {
    let Some(p) = Options::new(
        "alq",
        "tnp",
        &[
            "no-pager",
            "all",
            "full",
            "plain",
            "no-legend",
            "failed",
            "value",
            "user",
            "system",
            "quiet",
            "recursive",
            "reverse",
            "state=",
            "type=",
            "lines=",
            "property=",
            "output=",
        ],
    )
    .parse(arguments) else {
        return false;
    };
    if arguments.iter().any(|word| word == "--") {
        // The appended --no-pager would become an operand.
        return false;
    }
    let lines = p.value(&["n", "lines"]);
    p.operands.first().is_none_or(|verb| {
        [
            "status",
            "show",
            "cat",
            "is-active",
            "is-enabled",
            "is-failed",
            "is-system-running",
            "list-units",
            "list-unit-files",
            "list-timers",
            "list-sockets",
            "list-dependencies",
            "list-jobs",
            "get-default",
            "show-environment",
        ]
        .contains(&verb.as_str())
    }) && (lines.is_none() || number_within(lines, 1_000))
}

/// The journal must be bounded by a line count; following and maintenance stay manual.
fn journalctl_read_only(arguments: &[String]) -> bool {
    Options::new(
        "bkrxqalm",
        "nuptoSUg",
        &[
            "no-pager",
            "boot",
            "dmesg",
            "reverse",
            "catalog",
            "quiet",
            "utc",
            "no-hostname",
            "system",
            "user",
            "lines=",
            "unit=",
            "priority=",
            "since=",
            "until=",
            "output=",
            "identifier=",
            "grep=",
        ],
    )
    .parse(arguments)
    .is_some_and(|p| number_within(p.value(&["n", "lines"]), 2_000))
        // The appended --no-pager would become an operand.
        && !arguments.iter().any(|word| word == "--")
}

/// Interactive top takes over the screen; only batch (Linux) or logging (macOS) forms run.
fn top_read_only(arguments: &[String], platform: Platform) -> bool {
    let linux = || {
        Options::new("bcHiS1", "ndopuUEe", &[])
            .parse(arguments)
            .is_some_and(|p| {
                p.has(&["b"])
                    && p.operands.is_empty()
                    && number_within(p.value(&["n"]), 3)
                    && p.value(&["d"])
                        .is_none_or(|delay| delay == "0" || number_within(Some(delay), 5))
            })
    };
    let macos = || {
        let mut words = Vec::new();
        let mut iter = arguments.iter();
        while let Some(word) = iter.next() {
            if ["-stats", "-pid"].contains(&word.as_str()) {
                if iter.next().is_none() {
                    return false;
                }
            } else {
                words.push(word.clone());
            }
        }
        Options::new("", "lonOsU", &[])
            .parse(&words)
            .is_some_and(|p| {
                p.operands.is_empty()
                    && number_within(p.value(&["l"]), 3)
                    && p.value(&["s"])
                        .is_none_or(|delay| delay == "0" || number_within(Some(delay), 5))
            })
    };
    match platform {
        Platform::Linux => linux(),
        Platform::MacOs => macos(),
        _ => linux() || macos(),
    }
}

/// Letters of one pflag shorthand word up to the first value-taking letter, so `-Aw`
/// and `-f=true` reveal `w` and `f` while `-owide` is only `-o wide`.
fn pflag_letters(word: &str, valued: &str) -> String {
    let Some(cluster) = word.strip_prefix('-').filter(|rest| !rest.starts_with('-')) else {
        return String::new();
    };
    let mut letters = String::new();
    for letter in cluster.chars() {
        if letter == '=' {
            break;
        }
        letters.push(letter);
        if valued.contains(letter) {
            break;
        }
    }
    letters
}

/// Long pflag option name without its `=value`.
fn pflag_long(word: &str) -> Option<&str> {
    word.strip_prefix("--")
        .map(|long| long.split('=').next().unwrap_or(long))
}

fn container_read_only(arguments: &[String]) -> bool {
    let words: Vec<_> = arguments.iter().map(String::as_str).collect();
    // inspect is absent: container and image configuration carries environment values.
    let (verb, rest) = match words.as_slice() {
        [
            "network" | "volume" | "image" | "container" | "context",
            "ls" | "list",
            rest @ ..,
        ]
        | ["system", "df" | "info", rest @ ..]
        | [
            "compose",
            "ps" | "ls" | "images" | "top" | "version",
            rest @ ..,
        ] => ("list", rest),
        [
            "container" | "compose",
            verb @ ("logs" | "top" | "port"),
            rest @ ..,
        ] => (*verb, rest),
        [
            verb @ ("ps" | "images" | "version" | "info" | "logs" | "stats" | "top" | "port"),
            rest @ ..,
        ] => (*verb, rest),
        _ => return false,
    };
    match verb {
        "logs" => {
            !rest.iter().any(|word| {
                pflag_long(word) == Some("follow") || pflag_letters(word, "n").contains('f')
            }) && rest.iter().enumerate().any(|(index, word)| {
                let value = word.strip_prefix("--tail=").or_else(|| {
                    ["--tail", "-n"]
                        .contains(word)
                        .then(|| rest.get(index + 1).copied())
                        .flatten()
                });
                number_within(value, 2_000)
            })
        }
        // pflag lets a later `--no-stream=false` win.
        "stats" => {
            rest.contains(&"--no-stream")
                && !rest.iter().any(|word| word.starts_with("--no-stream="))
        }
        _ => true,
    }
}

/// Read verbs only; secrets, file input, watches and follows stay manual.
fn kubernetes_read_only(arguments: &[String]) -> bool {
    const VALUED: &str = "olLncsk";
    let words: Vec<_> = arguments.iter().map(String::as_str).collect();
    // Before the verb only these globals may appear: kubectl locates the verb by skipping
    // every flag's value, so an unknown value-taking flag could hide the real verb.
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if ["-n", "--namespace", "--context"].contains(word) {
            index += 2;
        } else if word.starts_with("--namespace=")
            || word.starts_with("--context=")
            || word.len() > 2 && word.starts_with("-n") && !word.starts_with("--")
        {
            index += 1;
        } else if word.starts_with('-') {
            return false;
        } else {
            break;
        }
    }
    let Some(verb) = words.get(index).copied() else {
        return false;
    };
    let after = &words[index + 1..];
    let read = match verb {
        "get" | "describe" | "top" | "explain" | "api-resources" | "api-versions" | "version"
        | "events" => true,
        "cluster-info" => !after.contains(&"dump"),
        "config" => after
            .first()
            .is_some_and(|sub| ["current-context", "get-contexts"].contains(sub)),
        "logs" => after.iter().enumerate().any(|(index, word)| {
            number_within(
                word.strip_prefix("--tail=").or_else(|| {
                    (*word == "--tail")
                        .then(|| after.get(index + 1).copied())
                        .flatten()
                }),
                2_000,
            )
        }),
        _ => false,
    };
    read && !words.iter().any(|word| {
        let letters = pflag_letters(word, VALUED);
        word.to_ascii_lowercase().contains("secret")
            || letters.contains(['f', 'w', 'k'])
            || pflag_long(word).is_some_and(|name| {
                ["follow", "watch", "watch-only", "filename", "kustomize"].contains(&name)
            })
    })
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
        if literals.iter().any(|literal| literal.contains(HOME)) {
            return Err(AutomaticRejection::Syntax);
        }
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

/// A Get-Content path outside credential stores. Wildcards could expand into one, so
/// they stay manual, except after -LiteralPath, which never expands them.
fn powershell_readable(path: &str, literal: bool) -> Option<()> {
    (!secret_path(path) && (literal || !path.contains(['*', '?', '[']))).then_some(())
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
            powershell_readable(words.next()?, lower == "-literalpath")?;
            has_path = true;
        } else if argument.starts_with('-') {
            return None;
        } else {
            powershell_readable(argument, false)?;
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

/// A finished word; a home-relative path may not climb out of the home directory,
/// whose depth is unknown here.
fn finished_word(word: String, home: bool) -> Option<Word> {
    if home && word.split('/').any(|part| part == "..") {
        return None;
    }
    Some(Word::Literal(word))
}

/// Whether `rest` continues a word at a boundary: end, whitespace or an operator.
fn at_boundary(rest: &std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    rest.clone()
        .next()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '|' | '&' | ';'))
}

/// Splits literal words and pipeline operators. Besides quoting, it accepts only shell
/// syntax whose effect is fixed before execution: a leading `~` or `~/` path,
/// `2>/dev/null`, and `#` comments, which are dropped.
fn literal_pipeline(command: &str) -> Option<Vec<(Vec<Word>, Option<String>)>> {
    let mut parts = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = None;
    let mut started = false;
    let mut home = false;
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
        } else if character == '#' && !started {
            while chars.peek().is_some_and(|c| !matches!(c, '\n' | '\r')) {
                chars.next();
            }
        } else if character == '~'
            && !started
            && !words.is_empty()
            && (chars.peek() == Some(&'/') || at_boundary(&chars))
        {
            word.push(HOME);
            started = true;
            home = true;
        } else if character == '2' && !started && !words.is_empty() && {
            let mut rest = chars.clone();
            ">/dev/null".chars().all(|c| rest.next() == Some(c)) && at_boundary(&rest)
        } {
            for _ in 0..">/dev/null".len() {
                chars.next();
            }
            words.push(Word::DiscardErrors);
        } else if matches!(character, '|' | '&' | ';' | '\n' | '\r') {
            if started {
                words.push(finished_word(std::mem::take(&mut word), home)?);
                started = false;
                home = false;
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
                words.push(finished_word(std::mem::take(&mut word), home)?);
                started = false;
                home = false;
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
        words.push(finished_word(word, home)?);
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
            Some("command 'uname' '-srm' ; command 'printf' 'shell=%s\\n' \"$0\" ; command 'pwd' ; command 'ls' '-la' | command 'head' '-n' '40'".into())
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
            Ok("command 'printf' '%s' '$(touch x)'".into())
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
        assert_eq!(automatic_read_only("env | grep -iE 'model|anthropic|openai|claude|gpt' || echo '未发现相关环境变量'"), Some("command 'env' | command 'grep' '-iE' 'model|anthropic|openai|claude|gpt' || command 'echo' '未发现相关环境变量'".into()));
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
            Some("command 'ls' '-la' 'a b'".into())
        );
        assert_eq!(automatic_read_only("pwd"), Some("command 'pwd'".into()));
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
        assert_eq!(automatic_read_only("pwd"), Some("command 'pwd'".into()));
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

    #[test]
    fn common_operations_diagnostics_are_eligible_on_their_own_platform() {
        let linux = [
            "free -h",
            "df -h /",
            "df -hT",
            "du -sh /var/log",
            "du -h --max-depth=1 /var | sort -h",
            "ps aux --sort=-%mem | head -n 10",
            "ps -eo pid,ppid,%cpu,%mem,cmd --sort=-%cpu | head -20",
            "top -bn1 | head -n 15",
            "top -b -n 1",
            "uptime",
            "cat /etc/os-release",
            "cat /proc/meminfo | grep -i mem",
            "grep -c processor /proc/cpuinfo",
            "grep -E 'error|fail' /var/log/syslog | tail -50",
            "grep -m 5 -e error /var/log/messages",
            "tail -n 100 /var/log/nginx/error.log",
            "ss -tlnp",
            "ss -s",
            "netstat -tulnp",
            "ip addr",
            "ip -br a",
            "ip route show",
            "ip route get 8.8.8.8",
            "systemctl status nginx",
            "systemctl --failed",
            "systemctl list-units --type=service --state=running",
            "systemctl is-active sshd",
            "journalctl -u nginx -n 50",
            "journalctl -p err -b -n 100 --no-pager",
            "dmesg -T | tail -n 30",
            "lsblk",
            "lscpu",
            "nproc",
            "vmstat 1 5",
            "hostname -I",
            "date '+%F %T'",
            "id -un",
            "last -n 10",
            "w",
            "lsof -i :80",
            "lsof -nP -iTCP -sTCP:LISTEN",
            "getent hosts example.com",
            "dig +short example.com",
            "ping -c 3 10.0.0.1",
            "docker ps -a",
            "docker logs --tail 100 web",
            "docker stats --no-stream",
            "kubectl get pods -A -o wide",
            "kubectl logs deploy/api --tail=200",
            "kubectl describe node worker-1",
            "kubectl -n kube-system get pods",
            "kubectl get pods -n kube-system -owide",
            "kubectl --namespace=prod describe deploy api",
            "kubectl get pods -o wide --show-labels",
            "kubectl get nodes -l role=worker",
            "ps -eo user,pid,%mem --sort=-%mem",
            "ps -eo user",
            "ps axo pid,comm",
            "ps -p 1 -o comm=",
            "printenv PATH HOME SHELL",
            "echo done",
            "cat ./config/app.yml",
            "cat /proc/loadavg",
            "netstat --numeric --tcp",
            "who am i",
            "crontab -l",
            "sysctl net.ipv4.ip_forward",
            "cut -d: -f1 /etc/passwd | sort | head -n 20",
            "uniq -c /tmp/list",
            "stat -c %s /var/log/syslog",
            "lsb_release -a",
            "timedatectl",
        ];
        for command in linux {
            assert!(
                prepare_read_only_for(command, Platform::Linux).is_ok(),
                "Linux diagnostic should auto-send: {command} -> {:?}",
                prepare_read_only_for(command, Platform::Linux)
            );
        }
        let macos = [
            "sw_vers",
            "sw_vers -productVersion",
            "vm_stat",
            "top -l 1 -n 10 -o mem",
            "top -l 1 -s 0 | head -n 12",
            "sysctl -n hw.ncpu hw.memsize",
            "df -h",
            "ps aux -m | head -n 10",
            "lsof -nP -iTCP -sTCP:LISTEN",
            "netstat -anv -p tcp",
            "ifconfig en0",
            "route -n get default",
            "launchctl list",
            "diskutil list",
            "log show --last 5m --predicate 'process == \"sshd\"' --style compact",
            "log show --last 1d",
            "scutil --dns",
            "system_profiler SPHardwareDataType",
            "uname -a",
        ];
        for command in macos {
            assert!(
                prepare_read_only_for(command, Platform::MacOs).is_ok(),
                "macOS diagnostic should auto-send: {command} -> {:?}",
                prepare_read_only_for(command, Platform::MacOs)
            );
        }
        assert_eq!(
            prepare_read_only_for("systemctl status nginx", Platform::Linux),
            Ok("command 'systemctl' 'status' 'nginx' '--no-pager'".into()),
            "pagers would take over the terminal"
        );
        assert_eq!(
            prepare_read_only_for("journalctl -n 20 --no-pager", Platform::Linux),
            Ok("command 'journalctl' '-n' '20' '--no-pager'".into())
        );
        assert_eq!(
            prepare_read_only("head -20 file"),
            Ok("command 'head' '-20' 'file'".into()),
            "arguments are sent exactly as validated"
        );
    }

    #[test]
    fn commands_for_another_platform_are_blocked_before_dispatch() {
        for (command, platform) in [
            ("free -h", Platform::MacOs),
            ("systemctl status nginx", Platform::MacOs),
            ("ss -tlnp", Platform::MacOs),
            ("ip addr", Platform::MacOs),
            ("journalctl -n 20", Platform::MacOs),
            ("cat /proc/meminfo", Platform::MacOs),
            ("grep MemTotal /proc/meminfo", Platform::MacOs),
            ("uptime; free -m", Platform::MacOs),
            ("vm_stat", Platform::Linux),
            ("sw_vers", Platform::Linux),
            ("launchctl list", Platform::Linux),
            ("free -h", Platform::Windows),
            ("vm_stat", Platform::Bsd),
        ] {
            assert!(
                matches!(
                    prepare_read_only_for(command, platform),
                    Err(AutomaticRejection::WrongPlatform { .. })
                ),
                "{command} must not auto-send on {platform:?}"
            );
        }
        // macOS and Linux spell batch top differently; each target only accepts its own.
        assert!(prepare_read_only_for("top -bn1", Platform::MacOs).is_err());
        assert!(prepare_read_only_for("top -l 1", Platform::Linux).is_err());
        assert!(prepare_read_only("top -bn1").is_ok() && prepare_read_only("top -l 1").is_ok());
        assert_eq!(
            prepare_read_only_for("free -h", Platform::MacOs).map_err(|error| error.to_string()),
            Err("free 不适用于 macOS，已阻止自动发送".into())
        );
    }

    #[test]
    fn widened_whitelist_still_rejects_writes_followers_and_interactive_forms() {
        for command in [
            "top",
            "top -n 1",
            "top -b",
            "top -bn100",
            "top -l 0",
            "tail -f /var/log/syslog",
            "tail -F file",
            "tail --follow file",
            "tail -n +1 file",
            "head -n -5 file",
            "journalctl -u nginx",
            "journalctl -f",
            "journalctl -n 20 -f",
            "journalctl --vacuum-size=1G",
            "journalctl -n 50 --rotate",
            "systemctl restart nginx",
            "systemctl stop nginx",
            "systemctl enable nginx",
            "systemctl daemon-reload",
            "systemctl -H host status",
            "dmesg -c",
            "dmesg -C",
            "dmesg -w",
            "dmesg -H",
            "ip addr add 10.0.0.1/24 dev eth0",
            "ip link set eth0 down",
            "ip route flush table main",
            "ip monitor",
            "ip route show flush",
            "ifconfig eth0 down",
            "ifconfig eth0 10.0.0.1",
            "route add default 10.0.0.1",
            "route delete default",
            "arp -d 10.0.0.1",
            "hostname newname",
            "date 010112002030",
            "date -s '2030-01-01'",
            "sysctl -w net.ipv4.ip_forward=1",
            "sysctl net.ipv4.ip_forward=1",
            "sysctl -p",
            "crontab -r",
            "crontab file",
            "crontab -u root",
            "ss -K dst 10.0.0.1",
            "ss -D dump",
            "ss -E",
            "netstat -c",
            "netstat -w 1",
            "netstat 2",
            "lsof -r 1 -i",
            "lsof -nPr",
            "vmstat 1",
            "vmstat 1 1000",
            "vm_stat 1",
            "free -s 1",
            "du /",
            "du -d 100 /",
            "uniq input output",
            "sort -o out file",
            "cat /dev/urandom",
            "head -c 10 /dev/zero",
            "tr a-z A-Z",
            "grep",
            "grep -f - file",
            "grep -r error /var/log",
            "file -C",
            "nslookup",
            "nslookup - 8.8.8.8",
            "ping 10.0.0.1",
            "ping -c 100 10.0.0.1",
            "ping -f -c 1 10.0.0.1",
            "docker logs web",
            "docker logs -f --tail 10 web",
            "docker stats",
            "docker rm web",
            "docker image rm nginx",
            "docker exec web ls",
            "docker run --rm alpine",
            "docker system prune",
            "kubectl delete pod web",
            "kubectl get pods -w",
            "kubectl get pods -Aw",
            "kubectl --user get delete pod web-0",
            "kubectl --as get delete namespace prod",
            "kubectl --token get exec -it web-0 -- sh",
            "kubectl -s get delete pod web",
            "kubectl config --user current-context view --raw",
            "kubectl logs web-0 --tail 10 -f=true",
            "kubectl get pods -w=true",
            "kubectl events -w=true",
            "kubectl get -f -",
            "kubectl describe -f //dev/tty",
            "kubectl get -k .",
            "docker logs --tail 10 -f=true web",
            "docker compose logs --tail 10 -f=1",
            "docker stats --no-stream --no-stream=false",
            "docker inspect web",
            "docker container inspect web",
            "tail +1f /var/log/syslog",
            "tail +1",
            "tail +1 /var/log/syslog",
            "head +5 file",
            "cat //dev/tty",
            "cat /./dev/stdin",
            "cat /dev/../dev/tty",
            "head -n 5 //dev/tty",
            "tail -c 5 /proc/self/fd/0",
            "cat /proc/self/environ",
            "cat /proc/1/environ",
            "cat /proc/kmsg",
            "grep -e x /proc/self/fd/0",
            "cat ../../../../dev/tty",
            "who /dev/tty",
            "who /var/run/utmp",
            "dig -f - example.com",
            "host -f - example.com",
            "netstat --cont -tn",
            "netstat --contin",
            "systemctl show --",
            "systemctl status -- --no-pager",
            "journalctl -n 10 --",
            "echo '\\033]52;c;c2VjcmV0\\007'",
            "echo 'x\\101'",
            "printenv AWS_SECRET_ACCESS_KEY",
            "printenv GITHUB_TOKEN",
            "printenv DB_PASSWORD",
            "printenv SSH_AUTH_SOCK",
            "ps eww",
            "ps auxe",
            "ps -E",
            "ps -Ef",
            "log show --last 3650d",
            "log show --last boot",
            "log show --last 2d",
            "crontab -l -u root",
            "kubectl -n get delete pod web",
            "kubectl get pods --watch=true",
            "kubectl logs -pf web --tail=10",
            "docker logs -ft --tail 100 web",
            "docker logs --tail 100 --follow=true web",
            "lsof -nPr1",
            "kubectl get secret db -o yaml",
            "kubectl describe secrets",
            "kubectl logs web",
            "kubectl logs -f web --tail=10",
            "kubectl exec web -- ls",
            "kubectl config view --raw",
            "kubectl cluster-info dump",
            "launchctl unload com.example",
            "launchctl bootout system/com.example",
            "diskutil eraseDisk JHFS+ x disk2",
            "log stream",
            "log show",
            "log erase",
            "scutil --set HostName x",
            "system_profiler",
            "getent shadow",
            "env sh",
            "printenv -0",
            "timedatectl set-timezone UTC",
            "hostnamectl set-hostname web",
            "which",
            "sw_vers --help",
        ] {
            assert!(
                prepare_read_only(command).is_err(),
                "command must require an explicit click: {command}"
            );
        }
    }
    #[test]
    fn bidirectional_controls_are_rejected_but_joiners_are_not() {
        for control in ('\u{202A}'..='\u{202E}').chain('\u{2066}'..='\u{2069}') {
            let command = format!("ls {control}-la");
            assert!(validate(&command).is_err(), "U+{:04X}", control as u32);
            assert_eq!(
                prepare_read_only(&command),
                Err(AutomaticRejection::Syntax),
                "U+{:04X}",
                control as u32
            );
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
    #[test]
    fn content_readers_never_print_credential_stores_automatically() {
        for command in [
            "cat /var/log/syslog",
            "cat README.md",
            "cat id_rsa.pub",
            "head -n 5 /home/u/id_ed25519.pub",
            "cat .env.d.ts.map/../README.md",
            "grep -e token README.md",
            "grep -i env .envrc.md",
            "wc -l .env",
            "file .env /home/u/.ssh/id_rsa",
            "ls -la /home/u/.ssh",
            "cat docs/credentials-guide.md",
            "cat src/shadow/README.md",
        ] {
            assert!(prepare_read_only(command).is_ok(), "{command}");
        }
        for (command, program) in [
            ("cat .env", "cat"),
            ("cat ./config/.env.production", "cat"),
            ("cat .ENV", "cat"),
            ("cat /home/u/.aws/credentials", "cat"),
            ("cat /home/u/.aws/config", "cat"),
            ("head -n 5 /home/u/.ssh/id_ed25519", "head"),
            ("tail -n 5 /Users/u/.SSH/config", "tail"),
            ("cat ../.ssh/known_hosts", "cat"),
            ("cat id_rsa", "cat"),
            ("cat ./ID_RSA", "cat"),
            ("cat id_ecdsa_sk", "cat"),
            ("cat backup/id_dsa.old", "cat"),
            ("grep x .git-credentials", "grep"),
            ("grep -e machine /home/u/.netrc", "grep"),
            ("grep password README.md .pgpass", "grep"),
            ("cat /home/u/.gnupg/pubring.kbx", "cat"),
            ("cat /home/u/.kube/config", "cat"),
            ("cat /home/u/.docker/config.json", "cat"),
            ("sort .npmrc", "sort"),
            ("uniq .pypirc", "uniq"),
            ("cut -c 1-5 .vault-token", "cut"),
            ("cat app-credentials.json", "cat"),
            ("cat /home/u/.cargo/credentials.toml", "cat"),
            ("cat certs/tls.pem", "cat"),
            ("cat server.KEY", "cat"),
            ("cat client.p12", "cat"),
            ("cat client.pfx", "cat"),
            ("cat /etc/shadow", "cat"),
            ("cat /etc/../etc/gshadow", "cat"),
            ("head -n 20 //etc/sudoers", "head"),
            ("cat /etc/sudoers.d/admin", "cat"),
            ("cat /run/credentials/app.service/token", "cat"),
            ("cat credentials.json.bak", "cat"),
            ("cat README.md /home/u/.aws/credentials", "cat"),
            ("cat README.md | grep x .env", "grep"),
        ] {
            assert_eq!(
                prepare_read_only(command),
                Err(AutomaticRejection::UnsupportedStage(program.into())),
                "{command}"
            );
        }
        assert!(prepare_powershell_read_only("Get-Content readme.md").is_ok());
        assert!(prepare_powershell_read_only("Get-Content -LiteralPath 'app[1].log'").is_ok());
        for command in [
            "Get-Content -Path 'app[1].log'",
            "Get-Content 'C:\\Users\\u\\.ssh\\id_rsa'",
            "gc .env",
            "cat -Path '~/.aws/credentials'",
            "type -LiteralPath 'C:\\certs\\server.pfx'",
            "Get-Content '~/.s*h/id_*'",
        ] {
            assert!(
                prepare_powershell_read_only(command).is_err(),
                "powershell command must stay manual: {command}"
            );
        }
    }
    #[test]
    fn home_paths_discarded_errors_and_comments_have_fixed_effects() {
        for (command, compiled) in [
            ("ls -la ~/logs", "command 'ls' '-la' ~/'logs'"),
            (
                "tail -n 50 ~/app.log",
                "command 'tail' '-n' '50' ~/'app.log'",
            ),
            ("ls ~", "command 'ls' ~"),
            ("ls ~/", "command 'ls' ~/''"),
            ("ls ~/'my dir'", "command 'ls' ~/'my dir'"),
            ("ls ~/\"it's\"", "command 'ls' ~/'it'\\''s'"),
            // Quoted, `~` stays a literal character.
            (
                "grep -e '~' README.md",
                "command 'grep' '-e' '~' 'README.md'",
            ),
            ("df -h 2>/dev/null", "command 'df' '-h' 2>/dev/null"),
            (
                "ls /missing 2>/dev/null | head -n 5",
                "command 'ls' '/missing' 2>/dev/null | command 'head' '-n' '5'",
            ),
            ("# 查看磁盘 (按目录)\ndf -h", "command 'df' '-h'"),
            ("df -h  # all mounts (incl. tmpfs)", "command 'df' '-h'"),
            (
                "df -h # a\n# b?\nuptime",
                "command 'df' '-h' ; command 'uptime'",
            ),
            ("grep '#' README.md", "command 'grep' '#' 'README.md'"),
            ("grep a#b README.md", "command 'grep' 'a#b' 'README.md'"),
        ] {
            assert_eq!(
                prepare_read_only(command).as_deref(),
                Ok(compiled),
                "{command}"
            );
        }
        for command in [
            "ls ~root",
            "ls ~+",
            "ls a~/x",
            "ls --dir=~/x",
            "~/bin/tool",
            "cat ~/../dev/tty",
            "ls ~/x/../../..",
            "ls 2>/tmp/out",
            "ls 2>/dev/nullx",
            "ls 2> /dev/null",
            "ls >/dev/null",
            "ls 2>>/dev/null",
            "ls '2>/dev/null'x 2>&1",
            "printf ~ x",
        ] {
            assert!(prepare_read_only(command).is_err(), "{command}");
        }
        // Credential checks see through the home directory.
        for command in [
            "cat ~/.ssh/id_rsa",
            "head -n 5 ~/.aws/credentials",
            "cat ~/.env",
        ] {
            assert_eq!(
                prepare_read_only(command),
                Err(AutomaticRejection::UnsupportedStage(
                    command.split(' ').next().unwrap_or_default().into()
                )),
                "{command}"
            );
        }
        // PowerShell spells these differently, so they stay manual there.
        for command in [
            "Get-Content ~/notes.txt",
            "Get-Content notes.txt 2>/dev/null",
        ] {
            assert_eq!(
                prepare_powershell_read_only(command),
                Err(AutomaticRejection::Syntax),
                "{command}"
            );
        }
        assert!(prepare_powershell_read_only("Get-Content notes.txt # first lines").is_ok());
    }

    #[test]
    fn file_operands_pass_the_same_device_checks_as_other_readers() {
        for command in ["file /bin/ls", "file -b README.md", "file /dev/null"] {
            assert!(prepare_read_only(command).is_ok(), "{command}");
        }
        for command in [
            "file /dev/tty",
            "file //dev/./stdin",
            "file /proc/self/environ",
            "file ../../../../dev/tty",
        ] {
            assert_eq!(
                prepare_read_only(command),
                Err(AutomaticRejection::UnsupportedStage("file".into())),
                "{command}"
            );
        }
        assert_eq!(
            prepare_read_only("file -"),
            Err(AutomaticRejection::TerminalInput("file".into()))
        );
        assert!(prepare_read_only("file").is_err());
    }
}
