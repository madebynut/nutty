//! Platform metadata belonging to one terminal transport, independent of output capture.

use std::path::Path;

use serde::Serialize;

/// A fixed, app-owned classification keeps remote text out of command-selection rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum Platform {
    #[serde(rename = "macOS")]
    MacOs,
    Linux,
    Windows,
    #[serde(rename = "BSD")]
    Bsd,
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Self::MacOs => "macOS",
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::Bsd => "BSD",
            Self::Unknown => "unknown",
        }
    }

    fn from_system_name(name: &str) -> Self {
        match name {
            "Darwin" | "macos" => Self::MacOs,
            "Linux" | "linux" => Self::Linux,
            "Windows_NT" | "windows" => Self::Windows,
            "FreeBSD" | "OpenBSD" | "NetBSD" | "DragonFly" | "freebsd" | "openbsd" | "netbsd"
            | "dragonfly" => Self::Bsd,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Local,
    Ssh,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Environment {
    pub target: Target,
    pub operating_system: Platform,
    pub kernel_release: Option<String>,
    pub architecture: Option<String>,
    pub startup_shell: Option<String>,
    /// Shell family or invocation observed in the active terminal, independent of login defaults.
    pub active_shell: Option<String>,
    pub last_observed_directory: Option<String>,
}

impl Environment {
    pub fn local(shell: &Path) -> Self {
        let environment = Self {
            target: Target::Local,
            operating_system: Platform::from_system_name(std::env::consts::OS),
            architecture: Some(std::env::consts::ARCH.into()),
            startup_shell: Some(shell.display().to_string()),
            ..Default::default()
        };
        #[cfg(unix)]
        let mut environment = environment;
        #[cfg(unix)]
        if let Ok(system) = nix::sys::utsname::uname() {
            environment.kernel_release = system.release().to_str().map(str::to_owned);
            environment.architecture = system.machine().to_str().map(str::to_owned);
        }
        environment
    }

    pub fn unknown_ssh() -> Self {
        Self {
            target: Target::Ssh,
            ..Default::default()
        }
    }

    /// Parse a complete bounded response; banners and partial frames cannot become facts.
    pub fn from_ssh_probe(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > 1024 || !bytes.ends_with(b"\n") {
            return None;
        }
        let mut lines = std::str::from_utf8(bytes).ok()?.lines();
        lines.find(|line| *line == "NUTTY_ENV_V1")?;
        let shell = lines.next()?;
        let system = lines.next()?;
        let release = lines.next()?;
        let architecture = lines.next()?;
        if [shell, system, release, architecture]
            .iter()
            .any(|value| value.len() > 256 || value.chars().any(char::is_control))
        {
            return None;
        }
        Some(Self {
            target: Target::Ssh,
            operating_system: Platform::from_system_name(system),
            startup_shell: (!shell.is_empty()).then(|| shell.into()),
            active_shell: None,
            kernel_release: (!release.is_empty()).then(|| release.into()),
            architecture: (!architecture.is_empty()).then(|| architecture.into()),
            last_observed_directory: None,
        })
    }

    /// The current-channel probe observes the selected target, not the gateway's login shell.
    pub fn from_current_shell_probe(bytes: &[u8]) -> Option<Self> {
        let mut environment = Self::from_ssh_probe(bytes)?;
        environment.active_shell = environment.startup_shell.take();
        Some(environment)
    }

    pub fn shell_name(&self) -> Option<&str> {
        self.active_shell
            .as_deref()
            .or(self.startup_shell.as_deref())
            .and_then(|shell| shell.rsplit(['/', '\\']).next())
            .map(|shell| {
                let shell = shell.trim_start_matches('-');
                shell
                    .len()
                    .checked_sub(4)
                    .filter(|&end| {
                        shell
                            .get(end..)
                            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
                    })
                    .map_or(shell, |end| &shell[..end])
            })
    }

    pub fn shell_is(&self, name: &str) -> bool {
        self.shell_name()
            .is_some_and(|shell| shell.eq_ignore_ascii_case(name))
    }

    pub fn supports_posix_commands(&self) -> bool {
        !["powershell", "pwsh", "cmd"]
            .iter()
            .any(|name| self.shell_is(name))
            && (self.operating_system != Platform::Windows
                || ["bash", "sh", "zsh", "dash", "ash"]
                    .iter()
                    .any(|name| self.shell_is(name)))
    }
}

/// Read-only metadata uses a separate exec channel and never enters the command history.
pub const SSH_PROBE: &str =
    "printf 'NUTTY_ENV_V1\\n%s\\n' \"$SHELL\"; uname -s; uname -r; uname -m";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_target_platform_and_shell_override_startup_without_cross_platform_commands() {
        let mut environment = Environment::from_current_shell_probe(
            b"NUTTY_ENV_V1\r\n-bash\r\nLinux\r\n6.8.0\r\nx86_64\r\n",
        )
        .expect("current Linux target");
        assert_eq!(environment.operating_system, Platform::Linux);
        assert!(environment.startup_shell.is_none());
        environment.startup_shell = Some("/bin/zsh".into());
        assert_eq!(environment.shell_name(), Some("bash"));
        assert!(crate::command::language_supported("BASH", &environment));
        assert!(!crate::command::language_supported(
            "powershell",
            &environment
        ));
        assert!(!crate::command::language_supported("json", &environment));
        environment.active_shell = Some("C:\\Program Files\\PowerShell\\pwsh.EXE".into());
        assert!(!environment.supports_posix_commands());
        assert!(crate::command::language_supported(
            "PowerShell",
            &environment
        ));
        assert!(!crate::command::language_supported("sh", &environment));
        environment.operating_system = Platform::Windows;
        environment.active_shell = Some("Bash.exe".into());
        assert!(environment.supports_posix_commands());
        environment.active_shell = None;
        environment.startup_shell = Some("CMD.EXE".into());
        assert!(!environment.supports_posix_commands());
    }

    #[test]
    fn remote_probe_is_bounded_complete_and_never_uses_the_clients_platform() {
        let bytes = b"Server banner\nNUTTY_ENV_V1\n/bin/bash\nLinux\n6.8.0\nx86_64\n";
        let environment = Environment::from_ssh_probe(bytes).expect("complete fixture probe");
        assert_eq!(environment.operating_system, Platform::Linux);
        assert_eq!(environment.startup_shell.as_deref(), Some("/bin/bash"));
        assert_eq!(environment.architecture.as_deref(), Some("x86_64"));
        for end in 0..bytes.len() {
            assert!(
                Environment::from_ssh_probe(&bytes[..end]).is_none(),
                "partial probe at {end}"
            );
        }
        for bad in [
            b"NUTTY_ENV_V1\n/bin/bash\nLinux\n6.8\nx86\0_64\n".as_slice(),
            b"/bin/zsh\n",
            b"NUTTY_ENV_V1\n/bin/bash\nLinux\n6.8\n\xff\n",
        ] {
            assert!(Environment::from_ssh_probe(bad).is_none());
        }
        assert!(Environment::from_ssh_probe(&vec![b'x'; 1025]).is_none());
        let unknown = Environment::unknown_ssh();
        assert_eq!(unknown.target, Target::Ssh);
        assert_eq!(unknown.operating_system, Platform::Unknown);
        assert!(unknown.architecture.is_none() && unknown.startup_shell.is_none());
    }
}
