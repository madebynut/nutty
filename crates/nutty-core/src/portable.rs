//! Authenticated configuration archives and non-executing OpenSSH configuration import.

use crate::config::{Authentication, Connection, Profile};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

const MAGIC: &[u8; 8] = b"NUTTYB01";
const MAX_ARCHIVE: usize = 8 * 1024 * 1024;
const HEADER: usize = 8 + 16 + 12;

#[derive(Serialize, Deserialize)]
pub struct Archive {
    pub profile: Profile,
    pub secrets: BTreeMap<String, String>,
}
impl Drop for Archive {
    fn drop(&mut self) {
        for value in self.secrets.values_mut() {
            value.zeroize();
        }
    }
}
pub struct Preview {
    pub archive: Archive,
    pub warnings: Vec<String>,
}

fn key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(
        (8..=4096).contains(&password.len()),
        "备份密码至少 8 字节，最多 4096 字节"
    );
    let params = argon2::Params::new(65_536, 3, 1, Some(32)).context("archive KDF parameters")?;
    let mut key = Zeroizing::new([0; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())?;
    Ok(key)
}
pub fn encrypt(archive: &Archive, password: &str) -> Result<Vec<u8>> {
    archive.profile.validate()?;
    validate_secrets(archive)?;
    let bytes = Zeroizing::new(serde_json::to_vec(archive)?);
    ensure!(bytes.len() <= MAX_ARCHIVE, "备份内容超过 8 MiB");
    let mut header = vec![0; HEADER];
    header[..8].copy_from_slice(MAGIC);
    getrandom::fill(&mut header[8..])?;
    let key = key(password, &header[8..24])?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())?;
    let nonce = Nonce::try_from(&header[24..36])?;
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &bytes,
                aad: &header,
            },
        )
        .map_err(|_| anyhow::anyhow!("备份加密失败"))?;
    header.extend_from_slice(&ciphertext);
    Ok(header)
}
pub fn decrypt(bytes: &[u8], password: &str) -> Result<Archive> {
    ensure!(
        (HEADER + 16..=MAX_ARCHIVE + HEADER + 16).contains(&bytes.len()) && &bytes[..8] == MAGIC,
        "不是受支持的 Nutty 备份文件"
    );
    let key = key(password, &bytes[8..24])?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())?;
    let nonce = Nonce::try_from(&bytes[24..36])?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &bytes[HEADER..],
                    aad: &bytes[..HEADER],
                },
            )
            .map_err(|_| anyhow::anyhow!("密码错误或备份文件已损坏"))?,
    );
    let archive: Archive = serde_json::from_slice(&plaintext)?;
    archive.profile.validate()?;
    validate_secrets(&archive)?;
    Ok(archive)
}
pub fn secret_names(profile: &Profile) -> HashSet<String> {
    let mut names: HashSet<_> = profile
        .connections
        .iter()
        .map(|item| item.id.to_string())
        .collect();
    names.extend(profile.ai_providers.iter().map(|item| item.secret_name()));
    names.extend(
        profile
            .proxies
            .iter()
            .map(|item| format!("proxy-{}", item.id)),
    );
    names.extend(
        profile
            .mcp_servers
            .iter()
            .map(|item| format!("integration-{}", item.id)),
    );
    names.extend(
        profile
            .agents
            .iter()
            .map(|item| format!("integration-{}", item.id)),
    );
    names
}
fn validate_secrets(archive: &Archive) -> Result<()> {
    let names = secret_names(&archive.profile);
    ensure!(
        archive
            .secrets
            .iter()
            .all(|(name, value)| names.contains(name) && value.len() <= 256 * 1024),
        "备份包含无效的凭据账户"
    );
    Ok(())
}
pub fn export(profile: Profile, password: &str, credentials: bool, path: &Path) -> Result<()> {
    let mut archive = Archive {
        profile,
        secrets: BTreeMap::new(),
    };
    if credentials {
        for name in secret_names(&archive.profile) {
            if let Some(value) = crate::config::optional_secret(&name)? {
                archive.secrets.insert(name, value.to_string());
            }
        }
    }
    write_new(path, &encrypt(&archive, password)?)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("创建备份文件，目标必须不存在")?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}
pub fn load(path: &Path, password: Option<&str>) -> Result<Preview> {
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= (MAX_ARCHIVE + HEADER + 16) as u64,
        "导入文件过大"
    );
    let mut bytes = Vec::new();
    file.take((MAX_ARCHIVE + HEADER + 17) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.starts_with(MAGIC) {
        let archive = decrypt(&bytes, password.context("请输入备份密码")?)?;
        Ok(Preview {
            warnings: activation_notices(&archive.profile),
            archive,
        })
    } else {
        let (connections, warnings) = openssh(
            std::str::from_utf8(&bytes)?,
            path.parent().unwrap_or(Path::new(".")),
        )?;
        Ok(Preview {
            archive: Archive {
                profile: Profile {
                    schema: 1,
                    connections,
                    ..Default::default()
                },
                secrets: BTreeMap::new(),
            },
            warnings,
        })
    }
}

/// Replaces the profile with an imported one without letting the file start programs.
///
/// MCP servers and agents stay enabled, and forwards keep connecting automatically,
/// only when an identical enabled record already exists locally. The local shell is
/// kept because the imported path names a program on another machine.
pub fn replace(current: &Profile, incoming: &Archive) -> Result<Archive> {
    let mut profile = incoming.profile.clone();
    for item in &mut profile.mcp_servers {
        let trusted = current.mcp_servers.contains(item);
        item.enabled &= trusted;
    }
    for item in &mut profile.agents {
        let trusted = current.agents.iter().any(|old| {
            old.id == item.id && old.enabled && old.name == item.name && old.process == item.process
        });
        item.enabled &= trusted;
    }
    for rule in &mut profile.forwards {
        let trusted = current.forwards.contains(rule);
        rule.on_connect &= trusted;
    }
    profile.preferences.shell = current.preferences.shell.clone();
    profile.validate()?;
    Ok(Archive {
        profile,
        secrets: incoming.secrets.clone(),
    })
}

/// Lists what importing will deliberately not activate, for the preview.
pub fn activation_notices(profile: &Profile) -> Vec<String> {
    let integrations = profile
        .mcp_servers
        .iter()
        .filter(|item| item.enabled)
        .count()
        + profile.agents.iter().filter(|item| item.enabled).count();
    let mut notices = Vec::new();
    if integrations > 0 {
        notices.push(format!(
            "{integrations} 个已启用的 MCP 服务或 Agent 不会随导入启用，请核对程序与参数后在设置中开启"
        ));
    }
    if profile.forwards.iter().any(|rule| rule.on_connect) {
        notices.push("导入的端口转发不会在连接时自动启动，请确认后重新开启".into());
    }
    notices
}

/// Merges records by stable identity/name, preserving existing records and their credentials.
pub fn merge(current: &Profile, incoming: &Archive) -> Result<Archive> {
    let mut result = current.clone();
    let mut imported_secrets = HashSet::new();
    macro_rules! records {
        ($field:ident, $name:expr) => {{
            let mut ids = HashMap::new();
            for item in &incoming.profile.$field {
                if let Some(existing) = result
                    .$field
                    .iter()
                    .find(|existing| existing.id == item.id || existing.name == item.name)
                {
                    ids.insert(item.id, existing.id);
                } else {
                    ids.insert(item.id, item.id);
                    imported_secrets.insert(($name)(item));
                    result.$field.push(item.clone());
                }
            }
            ids
        }};
    }
    let connection_ids = records!(connections, |item: &Connection| item.id.to_string());
    let proxy_ids = records!(proxies, |item: &crate::network::Proxy| format!(
        "proxy-{}",
        item.id
    ));
    records!(ai_providers, |item: &crate::provider::Provider| item
        .secret_name());
    records!(
        mcp_servers,
        |item: &crate::integration_config::McpServer| format!("integration-{}", item.id)
    );
    records!(agents, |item: &crate::integration_config::Agent| format!(
        "integration-{}",
        item.id
    ));
    let remap = |id: Option<Uuid>, ids: &HashMap<Uuid, Uuid>| {
        id.map(|id| ids.get(&id).copied().unwrap_or(id))
    };
    for item in &mut result.connections {
        if !current.connections.iter().any(|old| old.id == item.id) {
            item.jump = remap(item.jump, &connection_ids);
            item.proxy = remap(item.proxy, &proxy_ids);
        }
    }
    for item in &mut result.ai_providers {
        if !current.ai_providers.iter().any(|old| old.id == item.id) {
            item.proxy = remap(item.proxy, &proxy_ids);
        }
    }
    for item in &mut result.mcp_servers {
        if !current.mcp_servers.iter().any(|old| old.id == item.id)
            && let crate::integration_config::McpTransport::Http { proxy, .. } = &mut item.transport
        {
            *proxy = remap(*proxy, &proxy_ids);
        }
    }
    // Imported integrations name local programs; they stay off until reviewed here.
    for item in &mut result.mcp_servers {
        if !current.mcp_servers.iter().any(|old| old.id == item.id) {
            item.enabled = false;
        }
    }
    for item in &mut result.agents {
        if !current.agents.iter().any(|old| old.id == item.id) {
            item.enabled = false;
        }
    }
    for root in &incoming.profile.skills_roots {
        if !result.skills_roots.contains(root) {
            result.skills_roots.push(root.clone());
        }
    }
    for bookmark in &incoming.profile.file_bookmarks {
        let mut bookmark = bookmark.clone();
        bookmark.connection = remap(Some(bookmark.connection), &connection_ids)
            .expect("bookmark connection is present");
        if !result
            .file_bookmarks
            .iter()
            .any(|old| old.connection == bookmark.connection && old.path == bookmark.path)
        {
            if result
                .file_bookmarks
                .iter()
                .any(|old| old.id == bookmark.id)
            {
                bookmark.id = Uuid::new_v4();
            }
            result.file_bookmarks.push(bookmark);
        }
    }
    for rule in &incoming.profile.forwards {
        let mut rule = rule.clone();
        rule.connection =
            remap(Some(rule.connection), &connection_ids).expect("forward connection is present");
        // Imported forwarding must be explicitly enabled in its new environment.
        rule.on_connect = false;
        if !result.forwards.iter().any(|old| {
            old.connection == rule.connection
                && old.mode == rule.mode
                && old.bind == rule.bind
                && old.target == rule.target
        }) {
            if result.forwards.iter().any(|old| old.id == rule.id) {
                rule.id = Uuid::new_v4();
            }
            result.forwards.push(rule);
        }
    }
    result.validate()?;
    Ok(Archive {
        profile: result,
        secrets: incoming
            .secrets
            .iter()
            .filter(|(name, _)| imported_secrets.contains(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    })
}

struct SshBlock {
    selector: SshSelector,
    directives: Vec<(String, Vec<String>)>,
    active: bool,
}

enum SshSelector {
    Host(Vec<String>),
    Match(Vec<MatchCriterion>),
}

enum MatchCriterion {
    All,
    Host(String),
    OriginalHost(String),
    User(String),
    LocalUser(String),
}

struct ImportBudget {
    files: usize,
    bytes: usize,
}

pub fn openssh(text: &str, directory: &Path) -> Result<(Vec<Connection>, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut blocks = Vec::new();
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut budget = ImportBudget { files: 0, bytes: 0 };
    parse_ssh_file(
        text,
        directory,
        0,
        &mut budget,
        &mut seen,
        &mut warnings,
        &mut blocks,
        &mut names,
    )?;
    ensure!(
        names.len() <= 512 && !names.is_empty(),
        "配置中没有可导入的明确 Host 名称，或超过 512 个"
    );
    let local_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_else(|| directory.to_path_buf());
    let hostname = local_hostname();
    let uid = local_uid();
    let identities: HashMap<_, _> = names
        .iter()
        .map(|name| (name.clone(), Uuid::new_v4()))
        .collect();
    let mut connections = Vec::new();
    for name in names {
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        let mut user = local_user.clone();
        for block in &blocks {
            if !block.active || !selector_matches(&block.selector, &name, &user, &local_user) {
                continue;
            }
            for (key, items) in &block.directives {
                if key == "identityfile" || key == "certificatefile" {
                    values.entry(key.clone()).or_default().extend(items.clone());
                } else if !values.contains_key(key) {
                    values.insert(key.clone(), items.clone());
                    if key == "user"
                        && let Some(raw) = items.first()
                        && let Ok(expanded) = expand_token(
                            raw,
                            &name,
                            &name,
                            22,
                            &local_user,
                            &local_user,
                            &home,
                            &hostname,
                            uid,
                        )
                        && !expanded.is_empty()
                    {
                        user = expanded;
                    }
                }
            }
        }
        let mut destination = name.clone();
        if let Some(configured) = values.get("hostname").and_then(|items| items.first()) {
            match expand_token(
                configured,
                &name,
                &name,
                22,
                &user,
                &local_user,
                &home,
                &hostname,
                uid,
            ) {
                Ok(value) if !value.is_empty() => destination = value,
                Ok(_) => warnings.push(format!("{name} 的 HostName 展开后为空")),
                Err(reason) => warnings.push(format!("{name} 的 HostName {reason}")),
            }
        }
        let port = match values.get("port").and_then(|items| items.first()) {
            Some(value) => match expand_token(
                value,
                &name,
                &destination,
                22,
                &user,
                &local_user,
                &home,
                &hostname,
                uid,
            ) {
                Ok(value) => match value.parse::<u16>() {
                    Ok(port) if port > 0 => port,
                    _ => {
                        warnings.push(format!("{name} 的 Port 无效，已使用 22"));
                        22
                    }
                },
                Err(reason) => {
                    warnings.push(format!("{name} 的 Port {reason}"));
                    22
                }
            },
            None => 22,
        };
        if let Some(configured) = values.get("user").and_then(|items| items.first()) {
            match expand_token(
                configured,
                &name,
                &destination,
                port,
                &local_user,
                &local_user,
                &home,
                &hostname,
                uid,
            ) {
                Ok(value) if !value.is_empty() => user = value,
                Ok(_) => warnings.push(format!("{name} 的 User 展开后为空")),
                Err(reason) => warnings.push(format!("{name} 的 User {reason}")),
            }
        }
        let expand = |value: &str| {
            expand_token(
                value,
                &name,
                &destination,
                port,
                &user,
                &local_user,
                &home,
                &hostname,
                uid,
            )
        };
        let path_of = |value: &str| -> PathBuf {
            if let Some(value) = value.strip_prefix("~/") {
                home.join(value)
            } else {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                }
            }
        };
        let mut connection = Connection {
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            id: identities[&name],
            name: name.clone(),
            group: "OpenSSH".into(),
            host: destination.clone(),
            port,
            user: user.clone(),
            auth: Authentication::Agent,
            jump: None,
            ssh: Default::default(),
            proxy: None,
        };
        if let Some(items) = values.get("identityfile") {
            let mut paths = Vec::new();
            for item in items {
                match expand(item) {
                    Ok(value) if !value.is_empty() => paths.push(path_of(&value)),
                    Ok(_) => warnings.push(format!("{name} 的 IdentityFile 展开后为空")),
                    Err(reason) => warnings.push(format!("{name} 的 IdentityFile {reason}")),
                }
            }
            if let Some(path) = paths
                .iter()
                .find(|path| path.is_file())
                .cloned()
                .or_else(|| paths.into_iter().next())
            {
                let certificate = values.get("certificatefile").and_then(|items| {
                    let mut fallback = None;
                    for item in items {
                        let Ok(value) = expand(item) else {
                            continue;
                        };
                        if value.is_empty() {
                            continue;
                        }
                        let path = path_of(&value);
                        if path.is_file() {
                            return Some(path);
                        }
                        if fallback.is_none() {
                            fallback = Some(path);
                        }
                    }
                    fallback
                });
                connection.auth = Authentication::Key { path, certificate };
            }
        }
        if let Some(interval) = values
            .get("serveraliveinterval")
            .and_then(|items| items.first())
            && let Ok(value) = expand(interval)
            && let Ok(seconds) = value.parse()
        {
            connection.ssh.keepalive_seconds = seconds;
        }
        if let Some(count) = values
            .get("serveralivecountmax")
            .and_then(|items| items.first())
            && let Ok(value) = expand(count)
            && let Ok(max) = value.parse()
        {
            connection.ssh.keepalive_max = max;
        }
        if let Some(jump) = values.get("proxyjump").and_then(|items| items.first()) {
            match expand(jump) {
                Ok(value) if value == "none" => {}
                Ok(value) => {
                    if let Some(id) = identities.get(&value) {
                        connection.jump = Some(*id);
                    } else {
                        warnings.push(format!(
                            "{name} 的 ProxyJump {value} 未导入：请使用已定义的单个 Host 名称"
                        ));
                    }
                }
                Err(reason) => warnings.push(format!("{name} 的 ProxyJump {reason}")),
            }
        }
        connection.validate()?;
        connections.push(connection);
    }
    let profile = Profile {
        schema: 1,
        connections: connections.clone(),
        ..Default::default()
    };
    profile.validate()?;
    Ok((connections, warnings))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Include parsing threads one bounded import state without a shared owner"
)]
fn parse_ssh_file(
    text: &str,
    directory: &Path,
    depth: usize,
    budget: &mut ImportBudget,
    seen: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
    blocks: &mut Vec<SshBlock>,
    names: &mut Vec<String>,
) -> Result<()> {
    ensure!(text.len() <= 2 * 1024 * 1024, "OpenSSH 配置超过 2 MiB");
    ensure!(
        budget.bytes.saturating_add(text.len()) <= 2 * 1024 * 1024,
        "OpenSSH 配置超过 2 MiB"
    );
    budget.bytes += text.len();
    if blocks.is_empty() {
        blocks.push(SshBlock {
            selector: SshSelector::Host(vec!["*".into()]),
            directives: Vec::new(),
            active: true,
        });
    }
    let mut skipping = false;
    for (line, source) in text.lines().enumerate() {
        let source = source.trim();
        if source.is_empty() || source.starts_with('#') {
            continue;
        }
        let (key, value) = source
            .split_once([' ', '\t', '='])
            .context("OpenSSH 配置行缺少值")?;
        let key = key.to_ascii_lowercase();
        let tokens = ssh_tokens(value.trim_start_matches([' ', '\t', '=']))
            .with_context(|| format!("OpenSSH 第 {} 行", line + 1))?;
        if key == "host" {
            skipping = false;
            for name in &tokens {
                if !name.contains(['*', '?', '!']) && !names.contains(name) {
                    names.push(name.clone());
                }
            }
            blocks.push(SshBlock {
                selector: SshSelector::Host(tokens),
                directives: Vec::new(),
                active: true,
            });
            continue;
        }
        if key == "match" {
            match match_criteria(&tokens) {
                Ok(criteria) => {
                    skipping = false;
                    blocks.push(SshBlock {
                        selector: SshSelector::Match(criteria),
                        directives: Vec::new(),
                        active: true,
                    });
                }
                Err(reason) => {
                    skipping = true;
                    warnings.push(format!("跳过 Match 块（第 {} 行）：{reason}", line + 1));
                }
            }
            continue;
        }
        if skipping {
            continue;
        }
        if key == "include" {
            for token in tokens {
                read_include(
                    &token, directory, depth, line, budget, seen, warnings, blocks, names,
                )?;
            }
            continue;
        }
        if matches!(
            key.as_str(),
            "proxycommand" | "localcommand" | "remotecommand"
        ) {
            let label = match key.as_str() {
                "localcommand" => "LocalCommand",
                "remotecommand" => "RemoteCommand",
                _ => "ProxyCommand",
            };
            warnings.push(format!(
                "未导入 {label}（第 {} 行）；不会执行外部命令",
                line + 1
            ));
            continue;
        }
        if !matches!(
            key.as_str(),
            "hostname"
                | "port"
                | "user"
                | "identityfile"
                | "certificatefile"
                | "proxyjump"
                | "serveraliveinterval"
                | "serveralivecountmax"
        ) {
            continue;
        }
        if tokens.is_empty() {
            warnings.push(format!("未导入 {key}（第 {} 行）：缺少值", line + 1));
            continue;
        }
        let block = blocks.last_mut().context("SSH import block")?;
        if key == "identityfile" || key == "certificatefile" {
            if let Some((_, items)) = block.directives.iter_mut().find(|(name, _)| name == &key) {
                items.extend(tokens);
            } else {
                block.directives.push((key, tokens));
            }
        } else if tokens.len() == 1 {
            let value = tokens.into_iter().next().unwrap_or_default();
            if !block.directives.iter().any(|(name, _)| name == &key) {
                block.directives.push((key, vec![value]));
            }
        } else {
            warnings.push(format!("未导入 {key}（第 {} 行）：只接受一个值", line + 1));
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Include parsing threads one bounded import state without a shared owner"
)]
fn read_include(
    token: &str,
    directory: &Path,
    depth: usize,
    line: usize,
    budget: &mut ImportBudget,
    seen: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
    blocks: &mut Vec<SshBlock>,
    names: &mut Vec<String>,
) -> Result<()> {
    if depth >= 8 || budget.files >= 32 {
        warnings.push(format!(
            "未读取 Include（第 {} 行）：超过文件或层级限制",
            line + 1
        ));
        return Ok(());
    }
    let path = if let Some(value) = token.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| directory.to_path_buf())
            .join(value)
    } else {
        let path = PathBuf::from(token);
        if path.is_absolute() {
            path
        } else {
            directory.join(path)
        }
    };
    let mut files = Vec::new();
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(['*', '?']))
    {
        let pattern = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let parent = path.parent().unwrap_or(directory);
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if wildcard(&pattern, name) {
                    files.push(entry.path());
                }
            }
            files.sort();
        }
    } else {
        files.push(path);
    }
    if files.is_empty() {
        warnings.push(format!(
            "未读取 Include（第 {} 行）：没有匹配的文件",
            line + 1
        ));
        return Ok(());
    }
    for file in files {
        if budget.files >= 32 {
            warnings.push(format!(
                "未读取 Include（第 {} 行）：超过文件数量限制",
                line + 1
            ));
            break;
        }
        let Ok(metadata) = std::fs::metadata(&file) else {
            warnings.push(format!(
                "未读取 Include {}（第 {} 行）",
                file.display(),
                line + 1
            ));
            continue;
        };
        if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
            warnings.push(format!(
                "未读取 Include {}（第 {} 行）：不是普通文件或超过大小限制",
                file.display(),
                line + 1
            ));
            continue;
        }
        let key = file.canonicalize().unwrap_or(file.clone());
        if !seen.insert(key) {
            warnings.push(format!(
                "未读取 Include {}（第 {} 行）：循环引用",
                file.display(),
                line + 1
            ));
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            warnings.push(format!(
                "未读取 Include {}（第 {} 行）",
                file.display(),
                line + 1
            ));
            continue;
        };
        budget.files += 1;
        let parent = file.parent().unwrap_or(directory);
        parse_ssh_file(
            &text,
            parent,
            depth + 1,
            budget,
            seen,
            warnings,
            blocks,
            names,
        )?;
    }
    Ok(())
}

fn match_criteria(tokens: &[String]) -> Result<Vec<MatchCriterion>, String> {
    if tokens.is_empty() {
        return Err("缺少条件".into());
    }
    let mut criteria = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let key = tokens[index].to_ascii_lowercase();
        index += 1;
        match key.as_str() {
            "all" => criteria.push(MatchCriterion::All),
            "canonical" | "final" => {
                return Err("连接前无法判断 canonical 或 final".into());
            }
            "exec" => return Err("不会执行 exec".into()),
            "host" | "originalhost" | "user" | "localuser" => {
                let Some(pattern) = tokens.get(index) else {
                    return Err(format!("{key} 缺少模式"));
                };
                index += 1;
                criteria.push(match key.as_str() {
                    "host" => MatchCriterion::Host(pattern.clone()),
                    "originalhost" => MatchCriterion::OriginalHost(pattern.clone()),
                    "user" => MatchCriterion::User(pattern.clone()),
                    _ => MatchCriterion::LocalUser(pattern.clone()),
                });
            }
            _ => return Err(format!("不支持条件 {key}")),
        }
    }
    if criteria.is_empty() {
        return Err("缺少条件".into());
    }
    Ok(criteria)
}

fn selector_matches(selector: &SshSelector, name: &str, user: &str, local_user: &str) -> bool {
    match selector {
        SshSelector::Host(patterns) => {
            let includes = patterns
                .iter()
                .any(|pattern| !pattern.starts_with('!') && wildcard(pattern, name));
            let excludes = patterns.iter().any(|pattern| {
                pattern
                    .strip_prefix('!')
                    .is_some_and(|pattern| wildcard(pattern, name))
            });
            includes && !excludes
        }
        SshSelector::Match(criteria) => criteria.iter().all(|criterion| match criterion {
            MatchCriterion::All => true,
            MatchCriterion::Host(pattern) | MatchCriterion::OriginalHost(pattern) => {
                wildcard(pattern, name)
            }
            MatchCriterion::User(pattern) => wildcard(pattern, user),
            MatchCriterion::LocalUser(pattern) => wildcard(pattern, local_user),
        }),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Percent expansion needs the alias, destination, port, and local identity together"
)]
fn expand_token(
    value: &str,
    alias: &str,
    destination: &str,
    port: u16,
    user: &str,
    local_user: &str,
    home: &Path,
    hostname: &str,
    uid: Option<u32>,
) -> Result<String, String> {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let Some(token) = chars.next() else {
            return Err("百分号展开不完整".into());
        };
        match token {
            '%' => output.push('%'),
            'h' => output.push_str(destination),
            'n' | 'k' => output.push_str(alias),
            'p' => output.push_str(&port.to_string()),
            'r' => output.push_str(user),
            'd' => output.push_str(&home.to_string_lossy()),
            'u' => output.push_str(local_user),
            'l' => output.push_str(hostname),
            'L' => output.push_str(hostname.split('.').next().unwrap_or(hostname)),
            'i' => {
                let Some(uid) = uid else {
                    return Err("此平台无法展开 %i".into());
                };
                output.push_str(&uid.to_string());
            }
            'C' => {
                let material = format!("{hostname}{destination}{port}{user}");
                output.push_str(&hex_sha1(sha1(material.as_bytes())));
            }
            _ => return Err(format!("不支持百分号标记 %{token}")),
        }
    }
    if output.contains(['\0', '\r', '\n']) {
        return Err("展开结果包含控制字符".into());
    }
    Ok(output)
}

fn local_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is writable for the length passed to gethostname.
        let result = unsafe { nix::libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result == 0 {
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            if let Ok(name) = std::str::from_utf8(&buffer[..end])
                && !name.is_empty()
            {
                return name.to_string();
            }
        }
    }
    #[cfg(windows)]
    if let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.is_empty()
    {
        return name;
    }
    "localhost".into()
}

fn local_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid reads the process credential and takes no pointers.
        Some(unsafe { nix::libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state = [
        0x6745_2301_u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while (message.len() % 64) != 56 {
        message.push(0);
    }
    message.extend(bit_len.to_be_bytes());
    let (blocks, rest) = message.as_chunks::<64>();
    debug_assert!(rest.is_empty());
    for block in blocks {
        let mut words = [0_u32; 80];
        for (index, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            words[index] = u32::from_be_bytes(*chunk);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    let mut digest = [0_u8; 20];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..][..4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn hex_sha1(digest: [u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(40);
    for byte in digest {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0f) as usize] as char);
    }
    text
}
fn ssh_tokens(value: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = None;
    let mut escaped = false;
    let mut started = false;
    for ch in value.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            started = true;
        } else if ch == '\\' {
            escaped = true;
            started = true;
        } else if let Some(quote) = quoted {
            if ch == quote {
                quoted = None;
            } else {
                token.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quoted = Some(ch);
            started = true;
        } else if ch == '#' && !started {
            break;
        } else if ch.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut token));
                started = false;
            }
        } else {
            token.push(ch);
            started = true;
        }
    }
    ensure!(quoted.is_none() && !escaped, "未闭合的引号或转义");
    if started {
        tokens.push(token);
    }
    Ok(tokens)
}
fn wildcard(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let (pattern, value) = (pattern.as_bytes(), value.as_bytes());
    let (mut p, mut v, mut star, mut backtrack) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            backtrack = v;
        } else if let Some(index) = star {
            p = index + 1;
            backtrack += 1;
            v = backtrack;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archives_authenticate_password_header_and_content_without_overwriting_files() -> Result<()> {
        let archive = Archive {
            profile: Profile {
                schema: 1,
                ..Default::default()
            },
            secrets: BTreeMap::new(),
        };
        let encrypted = encrypt(&archive, "fixture-password")?;
        assert!(!encrypted.windows(7).any(|part| part == b"profile"));
        assert_eq!(decrypt(&encrypted, "fixture-password")?.profile.schema, 1);
        assert!(decrypt(&encrypted, "wrong-password").is_err());
        let mut corrupt = encrypted.clone();
        let end = corrupt.len() - 1;
        corrupt[end] ^= 1;
        assert!(decrypt(&corrupt, "fixture-password").is_err());
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("fixture.nutty");
        write_new(&path, &encrypted)?;
        assert!(write_new(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(path)?, encrypted);
        Ok(())
    }
    #[test]
    fn import_handles_inline_comments_quotes_and_independent_id_namespaces() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (mut connections, warnings) = openssh(
            "Host jump # comment\n HostName=jump.example # ignored\n User 'developer'\n IdentityFile \"key with spaces#literal\"\nHost work\n HostName work.example\n User developer\n ProxyJump jump\n",
            directory.path(),
        )?;
        assert!(warnings.is_empty());
        assert_eq!(connections.len(), 2);
        let Authentication::Key { path, .. } = &connections[0].auth else {
            panic!("quoted identity must be preserved");
        };
        assert_eq!(path, &directory.path().join("key with spaces#literal"));
        let shared = connections[0].id;
        let mut old = connections[0].clone();
        old.id = Uuid::new_v4();
        let proxy = crate::network::Proxy {
            id: shared,
            name: "test".into(),
            url: "socks5://localhost:1080".into(),
            username: String::new(),
        };
        connections[1].proxy = Some(shared);
        let incoming = Archive {
            profile: Profile {
                schema: 1,
                connections,
                proxies: vec![proxy],
                file_bookmarks: vec![crate::config::FileBookmark {
                    id: Uuid::new_v4(),
                    connection: shared,
                    path: "/home/developer".into(),
                }],
                ..Default::default()
            },
            secrets: BTreeMap::new(),
        };
        let current = Profile {
            schema: 1,
            connections: vec![old.clone()],
            ..Default::default()
        };
        let merged = merge(&current, &incoming)?;
        assert_eq!(merged.profile.connections[1].jump, Some(old.id));
        assert_eq!(merged.profile.connections[1].proxy, Some(shared));
        assert_eq!(merged.profile.file_bookmarks[0].connection, old.id);
        Ok(())
    }

    #[test]
    fn ssh_import_preserves_first_values_jump_links_and_existing_merge_records() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (connections, warnings) = openssh(
            "Host jump\n HostName jump.example\n User tunnel\nHost work\n HostName work.example\n User developer\n ProxyJump jump\n IdentityFile ~/.ssh/id_ed25519\nHost *\n User fallback\n ServerAliveInterval 20\nInclude ignored\nMatch exec unsafe\n ProxyCommand unsafe\n",
            directory.path(),
        )?;
        assert_eq!(connections[1].user, "developer");
        assert_eq!(connections[1].jump, Some(connections[0].id));
        assert_eq!(connections[1].ssh.keepalive_seconds, 20);
        assert_eq!(warnings.len(), 2);
        let mut existing = connections[0].clone();
        existing.id = Uuid::new_v4();
        existing.host = "existing.example".into();
        let current = Profile {
            schema: 1,
            connections: vec![existing.clone()],
            ..Default::default()
        };
        let incoming = Archive {
            profile: Profile {
                schema: 1,
                connections,
                ..Default::default()
            },
            secrets: BTreeMap::new(),
        };
        let merged = merge(&current, &incoming)?;
        assert_eq!(merged.profile.connections.len(), 2);
        assert_eq!(merged.profile.connections[0].host, "existing.example");
        assert_eq!(merged.profile.connections[1].jump, Some(existing.id));
        Ok(())
    }

    #[test]
    fn ssh_import_expands_safe_tokens_reads_includes_and_never_runs_commands() -> Result<()> {
        assert_eq!(
            hex_sha1(sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex_sha1(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("id_present"), "key")?;
        std::fs::write(
            directory.path().join("included"),
            "Host extra\n HostName extra.example\n",
        )?;
        let (connections, warnings) = openssh(
            "Host web\n HostName %h.example.com\n User deploy\n IdentityFile missing\n IdentityFile id_present\nInclude included\nMatch host web\n Port 2222\nMatch exec touch-ran\n User pwned\nHost extra\n ProxyCommand echo hi\n",
            directory.path(),
        )?;
        let web = connections
            .iter()
            .find(|item| item.name == "web")
            .expect("web");
        assert_eq!(web.host, "web.example.com");
        assert_eq!(web.user, "deploy");
        assert_eq!(web.port, 2222);
        let Authentication::Key { path, certificate } = &web.auth else {
            panic!("identity");
        };
        assert_eq!(path, &directory.path().join("id_present"));
        assert!(certificate.is_none());
        assert!(connections.iter().any(|item| item.name == "extra"));
        assert!(warnings.iter().any(|warning| warning.contains("不会执行")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("ProxyCommand"))
        );
        assert!(!directory.path().join("touch-ran").exists());
        let (unknown, warnings) = openssh(
            "Host bad\n HostName %Q.example\n Port abc\n",
            directory.path(),
        )?;
        assert_eq!(unknown[0].host, "bad");
        assert_eq!(unknown[0].port, 22);
        assert!(warnings.iter().any(|warning| warning.contains("百分号")));
        assert!(warnings.iter().any(|warning| warning.contains("Port")));
        Ok(())
    }
}

#[cfg(test)]
mod import_activation_tests {
    use super::*;
    use crate::integration_config::{Agent, McpServer, McpTransport, Process};

    fn server(name: &str) -> McpServer {
        McpServer {
            id: Uuid::new_v4(),
            name: name.into(),
            enabled: true,
            transport: McpTransport::Stdio {
                process: process("mcp-server"),
            },
        }
    }

    fn process(program: &str) -> Process {
        Process {
            program: program.into(),
            args: vec!["--stdio".into()],
            directory: std::env::temp_dir(),
            environment: false,
        }
    }

    fn agent(name: &str) -> Agent {
        Agent {
            id: Uuid::new_v4(),
            name: name.into(),
            enabled: true,
            process: process("agent"),
        }
    }

    fn connection() -> Connection {
        Connection {
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            id: Uuid::new_v4(),
            name: "server".into(),
            group: String::new(),
            host: "server.example".into(),
            port: 22,
            user: "alice".into(),
            auth: crate::config::Authentication::Agent,
            jump: None,
            ssh: Default::default(),
            proxy: None,
        }
    }

    fn forward(connection: Uuid) -> crate::forwarding::Rule {
        crate::forwarding::Rule {
            id: Uuid::new_v4(),
            connection,
            mode: crate::forwarding::Mode::Dynamic,
            bind: "127.0.0.1:1080".parse().expect("bind address"),
            target: None,
            on_connect: true,
        }
    }

    fn archive(profile: Profile) -> Archive {
        Archive {
            profile,
            secrets: BTreeMap::new(),
        }
    }

    #[test]
    fn merged_integrations_stay_off_until_enabled_here() -> Result<()> {
        let trusted = server("trusted");
        let mut current = Profile::default();
        current.mcp_servers.push(trusted.clone());
        let mut incoming = Profile::default();
        incoming
            .mcp_servers
            .extend([trusted.clone(), server("planted")]);
        incoming.agents.push(agent("planted agent"));
        let merged = merge(&current, &archive(incoming))?.profile.clone();
        let enabled = |name: &str| {
            merged
                .mcp_servers
                .iter()
                .find(|item| item.name == name)
                .map(|item| item.enabled)
        };
        assert_eq!(
            enabled("trusted"),
            Some(true),
            "existing records keep their state"
        );
        assert_eq!(enabled("planted"), Some(false));
        assert!(merged.agents.iter().all(|item| !item.enabled));
        Ok(())
    }

    #[test]
    fn replacing_keeps_the_local_shell_and_only_trusts_identical_records() -> Result<()> {
        let ssh = connection();
        let trusted_server = server("trusted");
        let trusted_agent = agent("trusted agent");
        let trusted_forward = forward(ssh.id);
        let mut current = Profile::default();
        current.connections.push(ssh.clone());
        current.mcp_servers.push(trusted_server.clone());
        current.agents.push(trusted_agent.clone());
        current.forwards.push(trusted_forward.clone());

        let mut changed_agent = trusted_agent.clone();
        changed_agent.process.program = "replaced-agent".into();
        let mut incoming = Profile::default();
        incoming.preferences.shell = "/tmp/planted-shell".into();
        incoming.connections.push(ssh.clone());
        incoming
            .mcp_servers
            .extend([trusted_server.clone(), server("planted")]);
        incoming.agents.push(changed_agent);
        incoming.forwards.extend([trusted_forward.clone(), {
            let mut rule = forward(ssh.id);
            rule.bind = "0.0.0.0:1081".parse().expect("bind address");
            rule
        }]);

        let replaced = replace(&current, &archive(incoming))?.profile.clone();
        assert_eq!(replaced.preferences.shell, current.preferences.shell);
        let server_enabled = |name: &str| {
            replaced
                .mcp_servers
                .iter()
                .find(|item| item.name == name)
                .map(|item| item.enabled)
        };
        assert_eq!(server_enabled("trusted"), Some(true));
        assert_eq!(server_enabled("planted"), Some(false));
        assert!(
            !replaced.agents[0].enabled,
            "a changed program is not trusted"
        );
        let on_connect = |id: Uuid| {
            replaced
                .forwards
                .iter()
                .find(|rule| rule.id == id)
                .map(|rule| rule.on_connect)
        };
        assert_eq!(on_connect(trusted_forward.id), Some(true));
        assert!(
            replaced
                .forwards
                .iter()
                .filter(|rule| !rule.on_connect)
                .count()
                == 1
        );
        Ok(())
    }

    #[test]
    fn encrypted_previews_report_what_will_not_be_activated() -> Result<()> {
        let ssh = connection();
        let mut profile = Profile::default();
        profile.connections.push(ssh.clone());
        profile.mcp_servers.push(server("planted"));
        profile.agents.push(agent("planted agent"));
        profile.forwards.push(forward(ssh.id));
        let notices = activation_notices(&profile);
        assert_eq!(notices.len(), 2);
        assert!(notices[0].starts_with("2 "));
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("backup.nutty");
        write_new(&path, &encrypt(&archive(profile), "correct horse")?)?;
        assert_eq!(load(&path, Some("correct horse"))?.warnings, notices);
        assert!(activation_notices(&Profile::default()).is_empty());
        Ok(())
    }
}
