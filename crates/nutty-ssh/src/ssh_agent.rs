//! Bounded SSH Agent discovery and public-identity inspection, without signing during probes.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result, ensure};
use russh::keys::{self, agent::client::AgentClient};

#[cfg(unix)]
type AgentStream = tokio::net::UnixStream;
#[cfg(windows)]
type AgentStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[derive(Clone, Debug)]
pub struct Identity {
    pub fingerprint: String,
    pub label: String,
}

#[derive(Debug)]
pub struct Info {
    pub socket: PathBuf,
    pub identities: Vec<Identity>,
}

pub async fn socket_path(configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    {
        // Finder-launched apps do not necessarily inherit the user's Shell environment.
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::process::Command::new("/bin/launchctl")
                .args(["getenv", "SSH_AUTH_SOCK"])
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        let value = std::str::from_utf8(&output.stdout)?.trim();
        if output.status.success() && !value.is_empty() && value.len() <= 4096 {
            return Ok(PathBuf::from(value));
        }
    }
    #[cfg(windows)]
    return Ok(PathBuf::from(r"\\.\pipe\openssh-ssh-agent"));
    #[cfg(not(windows))]
    anyhow::bail!("未发现 SSH Agent，请启动 Agent 并加载密钥，或选择 Agent 套接字");
}

#[cfg(any(unix, windows))]
pub async fn connect(configured: Option<&Path>) -> Result<(AgentClient<AgentStream>, PathBuf)> {
    let socket = socket_path(configured).await?;
    let client = tokio::time::timeout(Duration::from_secs(3), async {
        #[cfg(unix)]
        let client = AgentClient::connect_uds(&socket).await;
        #[cfg(windows)]
        let client = AgentClient::connect_named_pipe(&socket).await;
        client.context("无法连接 SSH Agent，请检查 Agent 是否运行及套接字路径")
    })
    .await
    .context("SSH Agent 连接超时")??;
    Ok((client, socket))
}

pub async fn probe(configured: Option<&Path>) -> Result<Info> {
    #[cfg(any(unix, windows))]
    {
        let (mut client, socket) = connect(configured).await?;
        let identities = tokio::time::timeout(Duration::from_secs(3), client.request_identities())
            .await
            .context("SSH Agent 获取密钥超时")?
            .context("SSH Agent 无法列出密钥，请检查是否已解锁")?;
        ensure!(
            identities.len() <= 128,
            "SSH Agent 密钥数量超过 128，请选择专用 Agent"
        );
        let identities = identities
            .into_iter()
            .map(|identity| {
                let key = identity.public_key();
                Identity {
                    fingerprint: key.fingerprint(keys::HashAlg::Sha256).to_string(),
                    label: format!(
                        "{} · {}",
                        key.algorithm(),
                        if matches!(identity, keys::agent::AgentIdentity::Certificate { .. }) {
                            crate::i18n::text("用户证书")
                        } else {
                            crate::i18n::text("密钥")
                        }
                    ),
                }
            })
            .collect();
        Ok(Info { socket, identities })
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("此平台的 SSH Agent 尚未接入，请使用密钥或密码");
}

#[cfg(all(test, unix))]
#[path = "ssh_agent_tests.rs"]
pub mod tests;
