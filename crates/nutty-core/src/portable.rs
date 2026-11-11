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
        Ok(Preview {
            archive: decrypt(&bytes, password.context("请输入备份密码")?)?,
            warnings: Vec::new(),
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
    patterns: Vec<String>,
    directives: Vec<(String, String)>,
}

pub fn openssh(text: &str, directory: &Path) -> Result<(Vec<Connection>, Vec<String>)> {
    ensure!(text.len() <= 2 * 1024 * 1024, "OpenSSH 配置超过 2 MiB");
    let mut warnings = Vec::new();
    let mut blocks = vec![SshBlock {
        patterns: vec!["*".into()],
        directives: Vec::new(),
    }];
    let mut names = Vec::new();
    let mut matched = false;
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
        let value = tokens.join(" ");
        if key == "match" {
            matched = true;
            warnings.push(format!("跳过 Match 块（第 {} 行）", line + 1));
            continue;
        }
        if key == "host" {
            matched = false;
            let patterns = tokens;
            for name in &patterns {
                if !name.contains(['*', '?', '!']) && !names.contains(name) {
                    names.push(name.clone());
                }
            }
            blocks.push(SshBlock {
                patterns,
                directives: Vec::new(),
            });
        } else if !matched {
            if matches!(
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
                if tokens.len() != 1 || value.contains('%') {
                    warnings.push(format!(
                        "未导入 {}（第 {} 行）：多值或百分号展开暂不支持",
                        key,
                        line + 1
                    ));
                    continue;
                }
                blocks
                    .last_mut()
                    .context("SSH import block")?
                    .directives
                    .push((key, value));
            } else if matches!(
                key.as_str(),
                "include" | "proxycommand" | "localcommand" | "remotecommand"
            ) {
                warnings.push(format!(
                    "未导入 {}（第 {} 行）；不会执行外部命令或读取 Include",
                    key,
                    line + 1
                ));
            }
        }
    }
    ensure!(
        names.len() <= 512 && !names.is_empty(),
        "配置中没有可导入的明确 Host 名称，或超过 512 个"
    );
    let identities: HashMap<_, _> = names
        .iter()
        .map(|name| (name.clone(), Uuid::new_v4()))
        .collect();
    let mut connections = Vec::new();
    for name in names {
        let mut values = HashMap::new();
        for SshBlock {
            patterns,
            directives,
        } in &blocks
        {
            let includes = patterns
                .iter()
                .any(|pattern| !pattern.starts_with('!') && wildcard(pattern, &name));
            let excludes = patterns.iter().any(|pattern| {
                pattern
                    .strip_prefix('!')
                    .is_some_and(|pattern| wildcard(pattern, &name))
            });
            if includes && !excludes {
                for (key, value) in directives {
                    values.entry(key.as_str()).or_insert(value.as_str());
                }
            }
        }
        let path = |value: &str| -> PathBuf {
            if let Some(value) = value.strip_prefix("~/") {
                dirs::home_dir()
                    .unwrap_or_else(|| directory.to_path_buf())
                    .join(value)
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
            host: values.get("hostname").unwrap_or(&name.as_str()).to_string(),
            port: values.get("port").unwrap_or(&"22").parse()?,
            user: values
                .get("user")
                .map(|value| value.to_string())
                .unwrap_or_else(|| {
                    std::env::var("USER")
                        .or_else(|_| std::env::var("USERNAME"))
                        .unwrap_or_default()
                }),
            auth: Authentication::Agent,
            jump: None,
            ssh: Default::default(),
            proxy: None,
        };
        if let Some(identity) = values.get("identityfile") {
            connection.auth = Authentication::Key {
                path: path(identity),
                certificate: values.get("certificatefile").map(|value| path(value)),
            };
        }
        if let Some(interval) = values.get("serveraliveinterval") {
            connection.ssh.keepalive_seconds = interval.parse()?;
        }
        if let Some(count) = values.get("serveralivecountmax") {
            connection.ssh.keepalive_max = count.parse()?;
        }
        if let Some(jump) = values.get("proxyjump").filter(|value| **value != "none") {
            if let Some(id) = identities.get(*jump) {
                connection.jump = Some(*id);
            } else {
                warnings.push(format!(
                    "{} 的 ProxyJump {} 未导入：请使用已定义的单个 Host 名称",
                    name, jump
                ));
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
}
