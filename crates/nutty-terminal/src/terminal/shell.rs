//! Validated OSC directory and prompt markers, scoped to one terminal.

use alacritty_terminal::index::Point;
use std::time::{Duration, Instant};

// Retry a failed handshake once, without continuously injecting commands into a shell.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5);
const BOOTSTRAP_ATTEMPTS: u8 = 2;

#[derive(Default)]
pub(super) struct Shell {
    pub environment: Option<crate::session_environment::Environment>,
    pub directory: Option<String>,
    pub target: Option<super::DirectoryTarget>,
    pub prompt: Option<Point>,
    pub integrated: bool,
    pub running: bool,
    pub input_pending: bool,
    pub track_directory: bool,
    location: Option<Location>,
    home: Option<String>,
    native_directory: bool,
    probe: Option<uuid::Uuid>,
    probed: bool,
    detected_shell: Option<&'static str>,
    pub bootstrap_input_pending: bool,
    bootstrap_started: Option<Instant>,
    bootstrap_attempts: u8,
}
impl Shell {
    pub fn new(track_directory: bool) -> Self {
        Self {
            track_directory,
            ..Default::default()
        }
    }
    pub fn osc(&mut self, bytes: &[u8], cursor: Point) {
        if let Some(value) = bytes.strip_prefix(b"7;") {
            if self.track_directory
                && let Some(path) = directory(value)
            {
                if let Ok(value) = std::str::from_utf8(value)
                    && let Ok(url) = url::Url::parse(value)
                    && let Some(host) = url.host_str()
                    && let Some(target) = &self.target
                    && target
                        .name
                        .rsplit_once('@')
                        .is_some_and(|(_, old)| old != host)
                {
                    let user = target.name.split('@').next().unwrap_or_default();
                    self.change_target(format!("{user}@{host}"));
                }
                self.directory = Some(path);
                self.native_directory = true;
            }
        } else if let Some(value) = bytes.strip_prefix(b"1337;CurrentDir=") {
            if self.track_directory
                && let Some(path) = raw_directory(value)
            {
                self.directory = Some(path);
                self.native_directory = true;
            }
        } else if let Some(value) = bytes.strip_prefix(b"777;nutty-") {
            self.probe_response(value);
        } else if let Some(value) = bytes
            .strip_prefix(b"0;")
            .or_else(|| bytes.strip_prefix(b"2;"))
        {
            if self.track_directory
                && let Ok(title) = std::str::from_utf8(value)
                && let Some(location) = Location::parse(title)
            {
                self.observe_location(location);
            }
        } else if let Some(value) = bytes.strip_prefix(b"133;") {
            match value.first() {
                Some(b'A') => {
                    // Some line editors redraw PS1 while an unsubmitted line still exists.
                    if !self.input_pending {
                        self.bootstrap_input_pending = false;
                    }
                    self.integrated = true;
                    self.prompt = None;
                    self.running = false;
                }
                Some(b'B') => {
                    self.bootstrap_started = None;
                    self.detected_shell = None;
                    self.probe = None;
                    self.integrated = true;
                    self.prompt = Some(cursor);
                    self.running = false;
                }
                Some(b'C') => {
                    self.integrated = true;
                    self.prompt = None;
                    self.running = true;
                }
                Some(b'D') => {
                    self.running = false;
                }
                _ => {}
            }
        }
    }

    fn observe_location(&mut self, location: Location) {
        if self
            .location
            .as_ref()
            .is_some_and(|old| old.target != location.target)
        {
            self.change_target(location.target.clone());
        }
        if self.target.is_none() {
            self.target = Some(super::DirectoryTarget {
                id: uuid::Uuid::new_v4(),
                name: location.target.clone(),
                transport_bound: false,
            });
        }
        if !self.native_directory {
            self.directory = if location.path.starts_with('/') {
                raw_directory(location.path.as_bytes())
            } else {
                self.home
                    .as_ref()
                    .filter(|_| location.path.starts_with('~'))
                    .map(|home| {
                        if location.path == "~" {
                            return home.clone();
                        }
                        format!(
                            "{}{}",
                            home.trim_end_matches('/'),
                            location.path.strip_prefix('~').unwrap_or_default()
                        )
                    })
            };
        }
        self.location = Some(location);
    }

    fn change_target(&mut self, name: String) {
        self.reset_location();
        self.target = Some(super::DirectoryTarget {
            id: uuid::Uuid::new_v4(),
            name,
            transport_bound: false,
        });
    }

    pub fn seed_target(&mut self, target: String, transport_bound: bool) {
        self.target = Some(super::DirectoryTarget {
            id: uuid::Uuid::new_v4(),
            name: target.clone(),
            transport_bound,
        });
        self.location = Some(Location {
            target,
            path: String::new(),
        });
    }

    pub fn reset_location(&mut self) {
        self.environment = None;
        self.directory = None;
        self.target = None;
        self.integrated = false;
        self.prompt = None;
        self.running = false;
        self.location = None;
        self.home = None;
        self.native_directory = false;
        self.probe = None;
        self.probed = false;
        self.detected_shell = None;
        self.bootstrap_started = None;
        self.bootstrap_attempts = 0;
    }

    pub fn integration_started(&mut self) {
        self.bootstrap_started = Some(Instant::now());
        self.bootstrap_attempts = self.bootstrap_attempts.saturating_add(1);
    }

    pub fn integration_pending(&self) -> bool {
        self.bootstrap_started.is_some()
    }

    pub fn retry_after_empty_enter(&mut self) {
        if !self.integrated && !self.integration_pending() && !self.bootstrap_input_pending {
            self.bootstrap_attempts = 0;
            self.probed = false;
        }
    }

    /// Only a complete, empty user@host shell prompt may trigger this current-channel query.
    pub fn probe_command(&mut self, prompt: &str) -> Option<String> {
        if let Some(started) = self.bootstrap_started {
            if started.elapsed() < BOOTSTRAP_TIMEOUT {
                self.detected_shell?;
            } else {
                self.probe = None;
                self.probed = false;
                self.detected_shell = None;
                self.bootstrap_started = None;
            }
        }
        let prompt = prompt.trim_end().strip_suffix(['$', '#', '%'])?.trim_end();
        if prompt.contains(['$', '#']) {
            return None;
        }
        self.observe_location(Location::parse(prompt)?);
        if let Some(shell) = self.detected_shell.take() {
            self.bootstrap_started = Some(Instant::now());
            return crate::shell_integration::remote_command(shell)
                .map(|command| format!(" {command}\r"));
        }
        if (self.integrated && self.prompt.is_some() && self.environment.is_some())
            || self.probed
            || self.bootstrap_attempts >= BOOTSTRAP_ATTEMPTS
        {
            return None;
        }
        let id = uuid::Uuid::new_v4();
        self.probe = Some(id);
        self.probed = true;
        self.integration_started();
        Some(format!(
            " printf '\\033]777;nutty-cwd;{id};%s\\007\\033]777;nutty-home;{id};%s\\007' \"$PWD\" \"$HOME\"; printf '\\033]777;nutty-env;{id};NUTTY_ENV_V1\\n\\n'; command uname -s; command uname -r; command uname -m; printf '\\007'; printf '\\033]777;nutty-shell;{id};%s|%s|%s\\007' \"$BASH_VERSION\" \"$ZSH_VERSION\" \"$version\"\r"
        ))
    }

    /// A full or closed input queue must not consume this target's one-time query.
    pub fn probe_not_sent(&mut self) {
        self.probe = None;
        self.probed = false;
        self.detected_shell = None;
        self.bootstrap_started = None;
        self.bootstrap_attempts = self.bootstrap_attempts.saturating_sub(1);
    }

    fn probe_response(&mut self, value: &[u8]) {
        let Ok(value) = std::str::from_utf8(value) else {
            return;
        };
        let mut fields = value.splitn(3, ';');
        let kind = fields.next();
        let id = fields
            .next()
            .and_then(|value| uuid::Uuid::parse_str(value).ok());
        if id.is_none() || id != self.probe {
            return;
        }
        let Some(value) = fields.next() else {
            return;
        };
        if kind == Some("env") {
            if let Some(environment) =
                crate::session_environment::Environment::from_current_shell_probe(value.as_bytes())
            {
                self.environment = Some(environment);
            }
            return;
        }
        if kind == Some("shell") {
            let parts: Vec<_> = value.split('|').collect();
            if parts.len() == 3
                && parts.iter().all(|p| {
                    p.len() <= 128
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || ".()-_".contains(c))
                })
            {
                self.detected_shell = Some(if !parts[0].is_empty() {
                    "bash"
                } else if !parts[1].is_empty() {
                    "zsh"
                } else if !parts[2].is_empty() {
                    "fish"
                } else {
                    "sh"
                });
                if let Some(environment) = &mut self.environment {
                    environment.active_shell = self.detected_shell.map(str::to_owned);
                }
            }
            return;
        }
        let Some(path) = self
            .track_directory
            .then(|| raw_directory(value.as_bytes()))
            .flatten()
        else {
            return;
        };
        match kind {
            Some("cwd") => self.directory = Some(path),
            Some("home") => self.home = Some(path),
            _ => {}
        }
    }
}

struct Location {
    target: String,
    path: String,
}
impl Location {
    fn parse(value: &str) -> Option<Self> {
        if value.len() > 4608 || value.chars().any(char::is_control) {
            return None;
        }
        let (target, path) =
            if let Some(bracketed) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                bracketed.split_once(' ')?
            } else {
                value.split_once(':')?
            };
        let (user, host) = target.split_once('@')?;
        if [user, host].iter().any(|part| {
            part.is_empty()
                || part.len() > 256
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        }) {
            return None;
        }
        let path = path.trim();
        if path.is_empty()
            || (path != "~"
                && !path.starts_with("~/")
                && !path.starts_with('/')
                && (!value.starts_with('[') || path.contains(['/', '\\'])))
        {
            return None;
        }
        Some(Self {
            target: target.into(),
            path: path.into(),
        })
    }
}

fn raw_directory(value: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(value).ok()?;
    let drive_absolute = path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && path.as_bytes().get(1) == Some(&b':')
        && path
            .as_bytes()
            .get(2)
            .is_some_and(|byte| matches!(byte, b'/' | b'\\'));
    ((path.starts_with('/') || path.starts_with(r"\\") || drive_absolute)
        && path.len() <= 4096
        && !path.chars().any(char::is_control))
    .then(|| path.to_owned())
}
fn directory(value: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(value).ok()?;
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "file"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let encoded = url.path();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    let path = String::from_utf8(decoded).ok()?;
    (path.starts_with('/') && path.len() <= 4096 && !path.chars().any(char::is_control))
        .then_some(path)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_directories_accept_drive_and_unc_but_reject_relative_and_controls() {
        for path in [
            r"C:\Users\alice\中文 space",
            r"\\server\share\project",
            "D:/work",
        ] {
            assert_eq!(raw_directory(path.as_bytes()).as_deref(), Some(path));
        }
        for path in ["C:relative", "work", "C:\\bad\x07name"] {
            assert_eq!(raw_directory(path.as_bytes()), None);
        }
    }

    #[test]
    fn target_facts_require_current_nonce_and_survive_disabled_directory_display() {
        use crate::session_environment::Platform;
        for track_directory in [true, false] {
            let mut shell = Shell::new(track_directory);
            shell.seed_target("admin@gateway".into(), true);
            // Native command boundaries are not platform observations.
            shell.osc(b"133;A", Point::default());
            shell.osc(b"133;B", Point::default());
            let query = shell
                .probe_command("admin@target:~$ ")
                .expect("target query");
            assert!(!query.trim_end().contains(['\r', '\n']));
            assert!(query.len() < 1024);
            let echo = super::super::bootstrap_echo::Echo::new(&query, 40, 15);
            assert!(echo.wire().len() < 1024, "query including echo envelope");
            let id = shell.probe.expect("nonce");
            let frame =
                format!("777;nutty-env;{id};NUTTY_ENV_V1\r\n\r\nLinux\r\n6.8.0\r\nx86_64\r\n");
            shell.osc(frame.as_bytes(), Point::default());
            shell.osc(
                format!("777;nutty-shell;{id};5.2||").as_bytes(),
                Point::default(),
            );
            let observed = shell.environment.as_ref().expect("target facts");
            assert_eq!(observed.operating_system, Platform::Linux);
            assert_eq!(observed.active_shell.as_deref(), Some("bash"));
            assert!(shell.probe_command("admin@target:~$ ").is_some());
            shell.osc(b"133;A", Point::default());
            shell.osc(b"133;B", Point::default());
            assert!(shell.probe_command("admin@target:~$ ").is_none());
            assert!(shell.probe_command("admin@other:~$ ").is_some());
            assert!(
                shell.environment.is_none(),
                "new target must discard old facts"
            );
            shell.osc(frame.as_bytes(), Point::default());
            assert!(
                shell.environment.is_none(),
                "late target frame must be ignored"
            );
            let current = shell.probe.expect("new target nonce");
            shell.osc(
                format!("777;nutty-env;{current};NUTTY_ENV_V1\n\nLinux\n6.8\n").as_bytes(),
                Point::default(),
            );
            assert!(
                shell.environment.is_none(),
                "incomplete facts must be ignored"
            );
            if !track_directory {
                assert!(shell.directory.is_none());
            }
        }
    }
    #[test]
    fn target_switch_discards_gateway_native_metadata_and_failed_queue_can_retry() {
        let mut shell = Shell::new(true);
        shell.seed_target("admin@gateway".into(), true);
        shell.osc(b"7;file://gateway/home/gateway", Point::default());
        assert_eq!(shell.directory.as_deref(), Some("/home/gateway"));
        shell.osc(b"0;admin@target: ~", Point::default());
        assert!(shell.directory.is_none());
        assert!(shell.probe_command("admin@target:~$ ").is_some());
        let first = shell.probe.expect("first token");
        shell.probe_not_sent();
        assert!(shell.probe_command("admin@target:~$ ").is_some());
        assert_ne!(shell.probe.expect("retry token"), first);
        shell.osc(
            format!("777;nutty-cwd;{first};/stale").as_bytes(),
            Point::default(),
        );
        assert!(shell.directory.is_none());
        assert!(shell.probe_command("admin@target:~$ pwd").is_none());
        assert!(shell.probe_command("admin@target:~$ echo $").is_none());
    }

    #[test]
    fn gateway_probe_belongs_to_the_current_target_and_home_paths_follow_prompts() {
        let mut shell = Shell::new(true);
        assert!(shell.probe_command("Choose a host: ").is_none());
        assert!(shell.probe_command("Password: ").is_none());
        let first = shell
            .probe_command("admin@target:~$ ")
            .expect("recognized target shell");
        let id = shell.probe.expect("query token");
        assert!(first.contains("\"$PWD\" \"$HOME\""));
        shell.osc(
            format!("777;nutty-cwd;{id};/home/admin").as_bytes(),
            Point::default(),
        );
        shell.osc(
            format!("777;nutty-home;{id};/home/admin").as_bytes(),
            Point::default(),
        );
        shell.osc(
            format!("777;nutty-shell;{id};5.2||").as_bytes(),
            Point::default(),
        );
        assert!(shell.probe_command("admin@target:~$ ").is_some());
        shell.osc(b"133;A", Point::default());
        shell.osc(b"133;B", Point::default());
        assert!(shell.probe_command("admin@target:~/日志$ ").is_none());
        assert_eq!(shell.directory.as_deref(), Some("/home/admin/日志"));
        shell
            .probe_command("ops@other:~$ ")
            .expect("new target needs its own query");
        assert!(
            shell.directory.is_none(),
            "other host must not inherit the previous directory"
        );
        shell.osc(
            format!("777;nutty-cwd;{id};/stale").as_bytes(),
            Point::default(),
        );
        assert!(
            shell.directory.is_none(),
            "old query responses cannot update a new target"
        );
    }

    #[test]
    fn native_directory_protocols_preserve_full_paths_while_installing_command_hooks() {
        let mut shell = Shell::new(true);
        shell.osc(b"1337;CurrentDir=/srv/a b/#name", Point::default());
        assert_eq!(shell.directory.as_deref(), Some("/srv/a b/#name"));
        assert!(shell.probe_command("admin@host:~$ ").is_some());
        shell.osc(b"1337;CurrentDir=/invalid\0path", Point::default());
        assert_eq!(shell.directory.as_deref(), Some("/srv/a b/#name"));
        shell.reset_location();
        shell.osc(
            b"7;file://host/srv/a%20b/%E4%B8%AD%E6%96%87",
            Point::default(),
        );
        assert_eq!(shell.directory.as_deref(), Some("/srv/a b/中文"));
        assert!(shell.probe_command("admin@host:~$ ").is_some());
    }

    #[test]
    fn bootstrap_retries_are_bounded_and_late_responses_cannot_select_a_shell() {
        let mut shell = Shell::new(false);
        assert!(shell.probe_command("[admin@host ~]$ ").is_some());
        let stale = shell.probe.expect("first probe");
        assert!(shell.probe_command("[admin@host ~]$ ").is_none());
        shell.bootstrap_started = Some(Instant::now() - BOOTSTRAP_TIMEOUT);
        assert!(shell.probe_command("[admin@host ~]$ ").is_some());
        shell.osc(
            format!("777;nutty-shell;{stale};5.2||").as_bytes(),
            Point::default(),
        );
        assert!(
            shell.detected_shell.is_none(),
            "expired probe cannot install hooks"
        );
        let current = shell.probe.expect("retry probe");
        shell.osc(
            format!("777;nutty-shell;{current};||").as_bytes(),
            Point::default(),
        );
        assert!(
            shell.probe_command("[admin@host ~]$ ").is_some(),
            "sh is detected even with directory display disabled"
        );
        shell.bootstrap_started = Some(Instant::now() - BOOTSTRAP_TIMEOUT);
        assert!(shell.probe_command("[admin@host ~]$ ").is_none());
        assert!(
            !shell.integration_pending(),
            "failed handshakes stop polling"
        );
        shell.retry_after_empty_enter();
        assert!(
            shell.probe_command("[admin@host ~]$ ").is_some(),
            "an explicit empty Enter retries an exhausted handshake"
        );
        shell.probe_not_sent();
        assert!(
            shell.probe_command("[admin@other logs]# ").is_some(),
            "new target gets a fresh handshake"
        );
        assert!(
            shell.directory.is_none(),
            "a basename cannot fabricate an absolute directory"
        );
    }

    #[test]
    fn directories_are_decoded_without_shell_or_control_characters() {
        assert_eq!(
            directory(b"file://host/tmp/a%20b/%E4%B8%AD%E6%96%87"),
            Some("/tmp/a b/中文".into())
        );
        for invalid in [
            b"file:///tmp/%00".as_slice(),
            b"file:///tmp/%0a",
            b"https://host/path",
            b"file://user@host/path",
            b"file:///tmp/%FF",
        ] {
            assert!(directory(invalid).is_none());
        }
    }
}
