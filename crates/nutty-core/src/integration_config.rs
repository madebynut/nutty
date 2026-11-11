//! Validated process, MCP and Agent configuration shared by storage and runtimes.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Process {
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub directory: PathBuf,
    #[serde(default)]
    pub environment: bool,
}
impl Process {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.program.as_os_str().is_empty()
                && self.args.len() <= 64
                && self
                    .args
                    .iter()
                    .all(|arg| arg.len() <= 4096 && !arg.contains('\0')),
            "程序或参数无效"
        );
        ensure!(
            self.directory.is_absolute() && self.directory.as_os_str().len() <= 4096,
            "程序工作目录须为绝对路径"
        );
        Ok(())
    }
    pub async fn command(&self, id: Uuid) -> Result<tokio::process::Command> {
        self.validate()?;
        ensure!(self.directory.is_dir(), "程序工作目录不存在");
        let mut command = tokio::process::Command::new(&self.program);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
            .args(&self.args)
            .current_dir(&self.directory)
            .kill_on_drop(true)
            .stderr(std::process::Stdio::null());
        if let Ok(path) = std::env::join_paths(crate::agent_discovery::search_directories()) {
            command.env("PATH", path);
        }
        // Reuse the installed CLI and its compatible native executable. Some npm
        // registries omit Codex's optional platform package from the adapter tree.
        if self.uses_codex_adapter()
            && std::env::var_os("CODEX_PATH").is_none()
            && let Some(codex) = crate::agent_discovery::codex_executable()
        {
            command.env("CODEX_PATH", codex);
        }
        if self.environment {
            let secret = tokio::task::spawn_blocking(move || {
                crate::config::secret(&format!("integration-{id}"))
            })
            .await??;
            let values: HashMap<String, String> = serde_json::from_str(&secret)?;
            validate_environment(&values)?;
            command.envs(values);
        }
        Ok(command)
    }

    pub fn uses_codex_adapter(&self) -> bool {
        self.program
            .file_stem()
            .is_some_and(|name| name == "codex-acp")
            || self.args.iter().any(|arg| {
                arg == "@agentclientprotocol/codex-acp"
                    || arg.starts_with("@agentclientprotocol/codex-acp@")
            })
    }

    pub fn uses_claude_adapter(&self) -> bool {
        self.program
            .file_stem()
            .is_some_and(|name| name == "claude-agent-acp")
            || self.args.iter().any(|arg| {
                arg == "@zed-industries/claude-agent-acp"
                    || arg.starts_with("@zed-industries/claude-agent-acp@")
            })
    }

    fn login_recipe(&self) -> Option<(PathBuf, Vec<String>)> {
        let name = self.program.file_stem()?.to_str()?;
        if self.uses_codex_adapter() || name == "codex" {
            Some((
                crate::agent_discovery::codex_executable()?,
                vec!["login".into()],
            ))
        } else if self.uses_claude_adapter() || name == "claude" {
            if let Some(cli) = crate::agent_discovery::cli_executable(&["claude", "claude.exe"]) {
                Some((cli, vec!["auth".into(), "login".into()]))
            } else {
                let mut args = self.args.clone();
                args.push("--cli".into());
                Some((self.program.clone(), args))
            }
        } else if matches!(name, "cursor-agent" | "agent")
            && self.args.iter().any(|arg| arg == "acp")
        {
            Some((self.program.clone(), vec!["login".into()]))
        } else if name == "gemini" {
            Some((self.program.clone(), Vec::new()))
        } else if name == "opencode" {
            Some((self.program.clone(), vec!["auth".into(), "login".into()]))
        } else {
            None
        }
    }

    pub fn login_command(&self) -> Option<String> {
        let (program, args) = self.login_recipe()?;
        let words = std::iter::once(program.to_str()?).chain(args.iter().map(String::as_str));
        #[cfg(windows)]
        return Some(format!(
            "& {}",
            words
                .map(|word| format!("'{}'", word.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        #[cfg(not(windows))]
        Some(
            words
                .map(crate::command::quote)
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    pub fn supports_terminal_login(&self) -> bool {
        self.login_recipe().is_some()
    }
}
pub fn validate_environment(values: &HashMap<String, String>) -> Result<()> {
    ensure!(
        values.len() <= 64
            && values.iter().all(|(name, value)| !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && value.len() <= 16 * 1024
                && !value.contains('\0')),
        "环境变量无效或过长"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "transport")]
pub enum McpTransport {
    Stdio {
        process: Process,
    },
    Http {
        url: String,
        #[serde(default)]
        bearer: bool,
        #[serde(default)]
        proxy: Option<Uuid>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServer {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
}
impl McpServer {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 128,
            "MCP 名称无效"
        );
        match &self.transport {
            McpTransport::Stdio { process } => process.validate()?,
            McpTransport::Http { url, .. } => {
                let endpoint = url::Url::parse(url)?;
                ensure!(
                    matches!(endpoint.scheme(), "http" | "https")
                        && endpoint.host_str().is_some()
                        && endpoint.password().is_none()
                        && endpoint.username().is_empty(),
                    "MCP URL 无效"
                );
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Agent {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub process: Process,
}
