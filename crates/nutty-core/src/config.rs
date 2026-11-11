//! Validated preferences, connection metadata, and protected credentials.

use std::{
    collections::HashMap,
    fmt, fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, OnceLock},
};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    Graphite,
    Light,
    Solarized,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::Graphite, Self::Light, Self::Solarized];

    pub fn label(self) -> &'static str {
        match self {
            Self::Graphite => crate::i18n::text("石墨"),
            Self::Light => crate::i18n::text("浅色"),
            Self::Solarized => "Solarized",
        }
    }
}

/// Geometry and interaction defaults shared by every terminal pane.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TerminalOptions {
    pub line_height: f32,
    pub letter_spacing: f32,
    pub padding_horizontal: f32,
    pub padding_vertical: f32,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub cursor_blink_ms: u64,
    pub cursor_width: f32,
    pub unfocused_cursor: bool,
    pub scroll_speed: f32,
    pub copy_on_select: bool,
    pub option_as_alt: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    #[default]
    Beam,
    Underline,
}

impl TerminalOptions {
    pub fn cursor_style(&self) -> alacritty_terminal::vte::ansi::CursorStyle {
        use alacritty_terminal::vte::ansi;
        ansi::CursorStyle {
            shape: match self.cursor_shape {
                CursorShape::Block => ansi::CursorShape::Block,
                CursorShape::Beam => ansi::CursorShape::Beam,
                CursorShape::Underline => ansi::CursorShape::Underline,
            },
            blinking: self.cursor_blink,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1. ..=2.5).contains(&self.line_height)
                && (0. ..=8.).contains(&self.letter_spacing)
                && (0. ..=80.).contains(&self.padding_horizontal)
                && (0. ..=80.).contains(&self.padding_vertical)
                && (200..=2000).contains(&self.cursor_blink_ms)
                && (1. ..=5.).contains(&self.cursor_width)
                && (0.5..=5.).contains(&self.scroll_speed),
            "终端排版或光标设置超出范围"
        );
        Ok(())
    }
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            line_height: 1.6,
            letter_spacing: 0.,
            padding_horizontal: 12.,
            padding_vertical: 8.,
            cursor_shape: CursorShape::Beam,
            cursor_blink: true,
            cursor_blink_ms: 600,
            cursor_width: 2.,
            unfocused_cursor: true,
            scroll_speed: 1.,
            copy_on_select: false,
            option_as_alt: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub appearance: Appearance,
    pub font_family: String,
    pub font_family_zh: String,
    pub font_size: f32,
    pub ui_font_family: String,
    pub ui_font_family_zh: String,
    pub ui_font_size: f32,
    pub agent_font_family: String,
    pub agent_font_family_zh: String,
    pub agent_font_size: f32,
    pub terminal: TerminalOptions,
    pub scrollback: usize,
    pub shell: PathBuf,
    pub directory: PathBuf,
    pub ai_model: String,
    pub ai_endpoint: String,
    pub language: crate::i18n::Language,
    pub track_directory: bool,
    pub record_history: bool,
    pub record_sessions: bool,
    pub restore_workspace: bool,
    /// Per-transfer limit in KiB/s; zero means unlimited.
    pub transfer_limit_kib: u64,
    pub transfer_concurrency: usize,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            appearance: Appearance::Graphite,
            font_family: crate::font_defaults::default_latin().into(),
            font_family_zh: crate::font_defaults::default_chinese().into(),
            font_size: 13.,
            ui_font_family: crate::font_defaults::default_latin().into(),
            ui_font_family_zh: crate::font_defaults::default_chinese().into(),
            ui_font_size: 14.,
            agent_font_family: crate::font_defaults::default_latin().into(),
            agent_font_family_zh: crate::font_defaults::default_chinese().into(),
            agent_font_size: 12.,
            terminal: TerminalOptions::default(),
            scrollback: 10_000,
            shell: std::env::var_os("SHELL")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    if cfg!(windows) {
                        PathBuf::from("powershell.exe")
                    } else {
                        PathBuf::from("/bin/sh")
                    }
                }),
            directory: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            ai_model: "gpt-5.2".into(),
            ai_endpoint: "https://api.openai.com/v1".into(),
            language: crate::i18n::Language::Chinese,
            track_directory: true,
            record_history: false,
            record_sessions: false,
            restore_workspace: true,
            transfer_limit_kib: 0,
            transfer_concurrency: 2,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Authentication {
    Password,
    Interactive,
    Key {
        path: PathBuf,
        certificate: Option<PathBuf>,
    },
    #[default]
    Agent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ConnectionKind {
    #[default]
    Ssh,
    Sftp,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Connection {
    #[serde(default)]
    pub kind: ConnectionKind,
    #[serde(default = "initial_directory")]
    pub initial_directory: String,
    pub id: Uuid,
    pub name: String,
    pub group: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Authentication,
    pub jump: Option<Uuid>,
    #[serde(default)]
    pub ssh: SshOptions,
    #[serde(default)]
    pub proxy: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct SshOptions {
    pub agent_socket: Option<PathBuf>,
    pub agent_identity: Option<String>,
    pub connect_timeout_seconds: u64,
    pub algorithms: crate::ssh_options::Algorithms,
    pub keepalive_seconds: u64,
    pub keepalive_max: usize,
    pub reconnect: bool,
    pub reconnect_attempts: u8,
}
impl Default for SshOptions {
    fn default() -> Self {
        Self {
            agent_socket: None,
            agent_identity: None,
            connect_timeout_seconds: 30,
            algorithms: Default::default(),
            keepalive_seconds: 30,
            keepalive_max: 3,
            reconnect: true,
            reconnect_attempts: 5,
        }
    }
}

fn initial_directory() -> String {
    ".".into()
}

impl Connection {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "请输入连接名称");
        ensure!(
            !self.host.trim().is_empty() && !self.host.contains(['\r', '\n', '\0']),
            "主机地址无效"
        );
        ensure!(self.port > 0, "端口必须介于 1 和 65535 之间");
        ensure!(!self.user.trim().is_empty(), "请输入用户名");
        ensure!(
            (1..=300).contains(&self.ssh.connect_timeout_seconds)
                && self.ssh.keepalive_seconds <= 3600
                && (1..=20).contains(&self.ssh.keepalive_max)
                && self.ssh.reconnect_attempts <= 10,
            "SSH 超时、保活或重连设置无效"
        );
        ensure!(
            !self.initial_directory.is_empty()
                && self.initial_directory.len() <= 4096
                && !self.initial_directory.contains(['\0', '\r', '\n']),
            "初始目录无效"
        );
        self.ssh.algorithms.validate()?;
        if let Some(socket) = &self.ssh.agent_socket {
            ensure!(
                socket.is_absolute()
                    && socket.as_os_str().len() <= 4096
                    && !socket.to_string_lossy().chars().any(char::is_control),
                "SSH Agent 套接字路径无效"
            );
        }
        if let Some(identity) = &self.ssh.agent_identity {
            ensure!(
                identity.strip_prefix("SHA256:").is_some_and(|hash| {
                    STANDARD_NO_PAD
                        .decode(hash)
                        .is_ok_and(|bytes| bytes.len() == 32)
                }),
                "SSH Agent 密钥指纹无效"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Profile {
    pub schema: u32,
    pub preferences: Preferences,
    pub connections: Vec<Connection>,
    pub ai_providers: Vec<crate::provider::Provider>,
    pub proxies: Vec<crate::network::Proxy>,
    pub mcp_servers: Vec<crate::integration_config::McpServer>,
    pub agents: Vec<crate::integration_config::Agent>,
    pub skills_roots: Vec<PathBuf>,
    pub file_bookmarks: Vec<FileBookmark>,
    pub forwards: Vec<crate::forwarding::Rule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileBookmark {
    pub id: Uuid,
    pub connection: Uuid,
    pub path: String,
}

pub struct Store {
    pub profile: Profile,
    pub directory: PathBuf,
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(directory) = std::env::var_os("NUTTY_DATA_DIR") {
        return Ok(PathBuf::from(directory));
    }
    Ok(dirs::config_dir()
        .context("system configuration directory is unavailable")?
        .join("nutty"))
}

impl Store {
    pub fn open(directory: PathBuf) -> Result<Self> {
        fs::create_dir_all(&directory).context("creating Nutty data directory")?;
        let path = directory.join("profile.json");
        if let Ok(metadata) = fs::metadata(&path) {
            ensure!(metadata.len() <= 2 * 1024 * 1024, "配置文件超过 2 MiB");
        }
        let mut profile = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<Profile>(&bytes).context("reading profile.json")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Profile::default(),
            Err(error) => return Err(error).context("reading profile.json"),
        };
        ensure!(
            (8. ..=32.).contains(&profile.preferences.font_size),
            "字体大小必须介于 8 和 32 之间"
        );
        if profile.schema == 0 && profile.ai_providers.is_empty() {
            profile
                .ai_providers
                .push(crate::provider::Provider::legacy(&profile.preferences));
        }
        profile.schema = 1;
        profile.validate()?;
        ensure!(
            profile.preferences.scrollback <= 100_000,
            "滚动历史最多为 100000 行"
        );
        Ok(Self { profile, directory })
    }

    pub fn save(&self) -> Result<()> {
        self.profile.validate()?;
        let bytes = serde_json::to_vec_pretty(&self.profile)?;
        let temporary = self
            .directory
            .join(format!(".profile-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            let target = self.directory.join("profile.json");
            match fs::read(&target) {
                Ok(previous) => {
                    let backup = self
                        .directory
                        .join(format!(".previous-{}.tmp", Uuid::new_v4()));
                    let backup_result = (|| -> Result<()> {
                        let mut file = options.open(&backup)?;
                        file.write_all(&previous)?;
                        file.sync_all()?;
                        fs::rename(&backup, self.directory.join("profile.previous.json"))?;
                        Ok(())
                    })();
                    if backup_result.is_err() {
                        let _ = fs::remove_file(&backup);
                    }
                    backup_result.context("saving previous profile snapshot")?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::rename(&temporary, target)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.context("saving Nutty profile")
    }
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        self.preferences.terminal.validate()?;
        ensure!(self.forwards.len() <= 128, "最多保存 128 条转发规则");
        let mut forward_ids = std::collections::HashSet::new();
        for rule in &self.forwards {
            rule.validate()?;
            ensure!(forward_ids.insert(rule.id), "转发规则身份重复");
            ensure!(
                self.connections
                    .iter()
                    .any(|c| c.id == rule.connection && c.kind == ConnectionKind::Ssh),
                "转发规则引用的 SSH 连接不存在"
            );
        }
        ensure!(self.schema <= 1, "配置版本高于当前应用支持范围");
        ensure!(
            self.preferences.transfer_limit_kib <= 1_048_576
                && (1..=8).contains(&self.preferences.transfer_concurrency),
            "传输限速不能超过 1048576 KiB/s"
        );
        let mut bookmark_ids = std::collections::HashSet::new();
        let mut bookmark_paths = std::collections::HashSet::new();
        ensure!(
            self.file_bookmarks.len() <= 1024,
            "最多保存 1024 个文件书签"
        );
        for bookmark in &self.file_bookmarks {
            ensure!(
                bookmark_ids.insert(bookmark.id)
                    && bookmark_paths.insert((bookmark.connection, bookmark.path.as_str()))
                    && self
                        .connections
                        .iter()
                        .any(|connection| connection.id == bookmark.connection)
                    && !bookmark.path.is_empty()
                    && bookmark.path.len() <= 4096
                    && !bookmark.path.contains(['\0', '\r', '\n']),
                "文件书签无效、重复或连接不存在"
            );
        }
        ensure!(
            (8. ..=32.).contains(&self.preferences.font_size)
                && (10. ..=24.).contains(&self.preferences.ui_font_size)
                && (10. ..=24.).contains(&self.preferences.agent_font_size)
                && self.preferences.scrollback <= 100_000
                && [
                    &self.preferences.font_family,
                    &self.preferences.font_family_zh,
                    &self.preferences.ui_font_family,
                    &self.preferences.ui_font_family_zh,
                    &self.preferences.agent_font_family,
                    &self.preferences.agent_font_family_zh
                ]
                .into_iter()
                .all(|family| !family.trim().is_empty()
                    && family.len() <= 256
                    && !family.contains(['\0', '\r', '\n'])),
            "字体或滚动历史设置无效"
        );
        let mut integration_ids = std::collections::HashSet::new();
        let mut integration_names = std::collections::HashSet::new();
        for server in &self.mcp_servers {
            ensure!(
                integration_ids.insert(server.id) && integration_names.insert(server.name.as_str()),
                "MCP ID 或名称重复"
            );
        }
        for agent in &self.agents {
            ensure!(
                integration_ids.insert(agent.id) && integration_names.insert(agent.name.as_str()),
                "Agent ID 或名称重复"
            );
        }
        let mut provider_ids = std::collections::HashSet::new();
        let mut provider_names = std::collections::HashSet::new();
        for provider in &self.ai_providers {
            ensure!(
                provider_ids.insert(provider.id) && provider_names.insert(provider.name.as_str()),
                "提供方 ID 或名称重复"
            );
        }
        let mut roots = std::collections::HashSet::new();
        for root in &self.skills_roots {
            ensure!(
                root.is_absolute() && roots.insert(root) && root.as_os_str().len() <= 4096,
                "Skills 目录须为唯一的绝对路径"
            );
        }
        ensure!(
            self.mcp_servers.len() <= 16
                && self.agents.len() <= 16
                && self.skills_roots.len() <= 16,
            "集成配置项目过多"
        );
        for server in &self.mcp_servers {
            server.validate()?;
            if let crate::integration_config::McpTransport::Http {
                proxy: Some(id), ..
            } = &server.transport
            {
                ensure!(
                    self.proxies.iter().any(|proxy| proxy.id == *id),
                    "MCP 代理不存在"
                );
            }
        }
        for agent in &self.agents {
            agent.process.validate()?;
            ensure!(
                !agent.name.trim().is_empty() && agent.name.len() <= 128,
                "Agent 名称无效"
            );
        }
        ensure!(
            self.connections.len() <= 4096
                && self.ai_providers.len() <= 64
                && self.proxies.len() <= 64,
            "配置项目过多"
        );
        let mut ids = std::collections::HashSet::new();
        for proxy in &self.proxies {
            proxy.validate()?;
            ensure!(ids.insert(proxy.id), "代理 ID 重复");
        }
        for provider in &self.ai_providers {
            provider.validate()?;
            if let Some(id) = provider.proxy {
                ensure!(ids.contains(&id), "提供方引用的代理不存在");
            }
        }
        let mut connection_ids = std::collections::HashSet::new();
        for connection in &self.connections {
            connection.validate()?;
            ensure!(connection_ids.insert(connection.id), "连接 ID 重复");
            if let Some(id) = connection.proxy {
                ensure!(ids.contains(&id), "连接引用的代理不存在");
            }
        }
        for connection in &self.connections {
            if let Some(id) = connection.jump {
                ensure!(
                    connection_ids.contains(&id) && id != connection.id,
                    "跳板引用无效"
                );
            }
        }
        for connection in &self.connections {
            let mut route = std::collections::HashSet::new();
            let mut current = Some(connection.id);
            while let Some(id) = current {
                ensure!(
                    route.insert(id) && route.len() <= 16,
                    "跳板连接存在循环或超过 16 跳"
                );
                current = self
                    .connections
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| item.jump);
            }
        }
        Ok(())
    }
}

pub fn secret(name: &str) -> Result<Zeroizing<String>> {
    cached_secret(secret_cache(), name, || {
        Ok(Zeroizing::new(
            keyring::Entry::new("app.nutty", name)
                .and_then(|entry| entry.get_password())
                .map_err(CredentialError::from)?,
        ))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    Missing,
    Unavailable,
}
impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "未保存凭据，请在对应配置中填写密钥或密码",
            Self::Unavailable => "系统钥匙串访问失败；已暂停重复授权，请解锁后重试，或重新配置凭据",
        })
    }
}
impl std::error::Error for CredentialError {}
impl From<keyring::Error> for CredentialError {
    fn from(error: keyring::Error) -> Self {
        match error {
            keyring::Error::NoEntry => Self::Missing,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Default)]
struct CredentialCache {
    values: HashMap<String, Zeroizing<String>>,
    session: HashMap<String, Zeroizing<String>>,
    failures: HashMap<String, CredentialError>,
    reads: HashMap<String, Arc<CredentialRead>>,
}

type CredentialResult = std::result::Result<Zeroizing<String>, CredentialError>;

#[derive(Default)]
struct CredentialRead {
    result: Mutex<Option<CredentialResult>>,
    ready: Condvar,
}
impl CredentialRead {
    fn finish(&self, value: CredentialResult) -> Result<()> {
        let mut result = self
            .result
            .lock()
            .map_err(|_| CredentialError::Unavailable)?;
        // Saving a replacement or exiting may already have completed this read.
        if result.is_none() {
            *result = Some(value);
            self.ready.notify_all();
        }
        Ok(())
    }

    fn wait(&self) -> Result<Zeroizing<String>> {
        let result = self
            .result
            .lock()
            .map_err(|_| CredentialError::Unavailable)?;
        let result = self
            .ready
            .wait_while(result, |value| value.is_none())
            .map_err(|_| CredentialError::Unavailable)?;
        result
            .as_ref()
            .expect("completed credential read has a result")
            .clone()
            .map_err(Into::into)
    }
}

impl CredentialCache {
    fn value(&self, name: &str) -> Option<Zeroizing<String>> {
        self.session
            .get(name)
            .or_else(|| self.values.get(name))
            .cloned()
    }

    fn retry(&mut self) {
        self.failures.clear();
    }

    fn remove(&mut self, name: &str) {
        self.values.remove(name);
        self.session.remove(name);
        self.failures.remove(name);
        if let Some(read) = self.reads.remove(name) {
            let _ = read.finish(Err(CredentialError::Missing));
        }
    }

    fn clear(&mut self) {
        self.values.clear();
        self.session.clear();
        self.failures.clear();
        for read in self.reads.drain().map(|(_, read)| read) {
            let _ = read.finish(Err(CredentialError::Unavailable));
        }
    }

    fn insert(&mut self, name: &str, value: &str) {
        // Bound session memory even when opening many saved SSH connections.
        const CAPACITY: usize = 128;
        const MAX_VALUE: usize = 256 * 1024;
        if value.len() > MAX_VALUE {
            return;
        }
        if self.values.len() >= CAPACITY && !self.values.contains_key(name) {
            self.values.clear();
        }
        self.values
            .insert(name.into(), Zeroizing::new(value.into()));
        self.failures.remove(name);
        if let Some(read) = self.reads.remove(name) {
            let _ = read.finish(Ok(Zeroizing::new(value.into())));
        }
    }
}
fn cached_secret(
    cache: &Mutex<CredentialCache>,
    name: &str,
    load: impl FnOnce() -> Result<Zeroizing<String>>,
) -> Result<Zeroizing<String>> {
    let (read, owner) = {
        let mut cache = cache.lock().map_err(|_| CredentialError::Unavailable)?;
        if let Some(value) = cache.value(name) {
            tracing::debug!("credential memory cache hit");
            return Ok(value);
        }
        if let Some(error) = cache.failures.get(name) {
            tracing::debug!("credential access awaits explicit retry");
            return Err((*error).into());
        }
        if let Some(read) = cache.reads.get(name) {
            (read.clone(), false)
        } else {
            // Failed accounts never evict one another and reopen authorization dialogs.
            // The limit bounds bookkeeping independently of the secret memory budget.
            const MAX_ATTEMPTS: usize = 4096;
            ensure!(
                cache.failures.len() + cache.reads.len() < MAX_ATTEMPTS,
                CredentialError::Unavailable
            );
            let read = Arc::new(CredentialRead::default());
            cache.reads.insert(name.into(), read.clone());
            (read, true)
        }
    };
    if owner {
        // Neither a system prompt nor waiting for one holds the global cache lock.
        tracing::debug!("reading credential from system storage");
        let value = load().map_err(|error| {
            error
                .downcast_ref::<CredentialError>()
                .copied()
                .unwrap_or(CredentialError::Unavailable)
        });
        let mut cache = cache.lock().map_err(|_| CredentialError::Unavailable)?;
        if cache
            .reads
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, &read))
        {
            cache.reads.remove(name);
            match &value {
                Ok(value) => cache.insert(name, value),
                Err(error) => {
                    cache.failures.insert(name.into(), *error);
                }
            }
            read.finish(value)?;
        }
    }
    let result = read.wait();
    let cache = cache.lock().map_err(|_| CredentialError::Unavailable)?;
    cache.value(name).map_or(result, Ok)
}
fn secret_cache() -> &'static Mutex<CredentialCache> {
    static CACHE: OnceLock<Mutex<CredentialCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(CredentialCache::default()))
}
pub fn use_session_secret(name: &str, value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 256 * 1024,
        "凭据为空或过长"
    );
    let mut cache = secret_cache()
        .lock()
        .map_err(|_| CredentialError::Unavailable)?;
    ensure!(
        cache.session.len() < 64 || cache.session.contains_key(name),
        "临时密钥数量达到上限，请重启应用后再配置"
    );
    cache
        .session
        .insert(name.into(), Zeroizing::new(value.into()));
    cache.failures.remove(name);
    if let Some(read) = cache.reads.remove(name) {
        read.finish(Ok(Zeroizing::new(value.into())))?;
    }
    Ok(())
}
/// Allow another storage attempt only after an explicit user recovery action.
pub fn retry_secret_access() {
    if let Ok(mut cache) = secret_cache().lock() {
        cache.retry();
    }
}
pub fn clear_secret_cache() {
    if let Ok(mut cache) = secret_cache().lock() {
        cache.clear();
    }
}
pub fn optional_secret(name: &str) -> Result<Option<Zeroizing<String>>> {
    match keyring::Entry::new("app.nutty", name)?.get_password() {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(CredentialError::from(error).into()),
    }
}

pub fn save_secret(name: &str, value: &str) -> Result<()> {
    keyring::Entry::new("app.nutty", name)
        .and_then(|entry| entry.set_password(value))
        .map_err(CredentialError::from)?;
    Ok(())
}

trait Credentials {
    fn read(&mut self, name: &str) -> Result<Option<Zeroizing<String>>>;
    fn write(&mut self, name: &str, value: &str) -> Result<()>;
    fn remove(&mut self, name: &str) -> Result<()>;
}
struct Keychain;
impl Credentials for Keychain {
    fn read(&mut self, name: &str) -> Result<Option<Zeroizing<String>>> {
        optional_secret(name)
    }
    fn write(&mut self, name: &str, value: &str) -> Result<()> {
        save_secret(name, value)
    }
    fn remove(&mut self, name: &str) -> Result<()> {
        match keyring::Entry::new("app.nutty", name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
pub fn save_with_secrets(store: &Store, updates: &[(String, Zeroizing<String>)]) -> Result<()> {
    save_transaction(store, updates, &mut Keychain)?;
    if let Ok(mut cache) = secret_cache().lock() {
        for (name, value) in updates {
            cache.session.remove(name);
            cache.insert(name, value);
        }
    }
    Ok(())
}
fn save_transaction(
    store: &Store,
    updates: &[(String, Zeroizing<String>)],
    credentials: &mut impl Credentials,
) -> Result<()> {
    save_transaction_changes(store, updates, &[], credentials)
}

pub fn save_with_secret_removals(
    store: &Store,
    updates: &[(String, Zeroizing<String>)],
    removals: &[String],
) -> Result<()> {
    if removals.is_empty() {
        return save_with_secrets(store, updates);
    }
    save_transaction_changes(store, updates, removals, &mut Keychain)?;
    if let Ok(mut cache) = secret_cache().lock() {
        for name in removals {
            cache.remove(name);
        }
        for (name, value) in updates {
            cache.session.remove(name);
            cache.insert(name, value);
        }
    }
    Ok(())
}

fn save_transaction_changes(
    store: &Store,
    updates: &[(String, Zeroizing<String>)],
    removals: &[String],
    credentials: &mut impl Credentials,
) -> Result<()> {
    store.profile.validate()?;
    let mut names = std::collections::HashSet::new();
    ensure!(
        updates
            .iter()
            .map(|(name, _)| name)
            .chain(removals.iter())
            .all(|name| names.insert(name)),
        "凭据变更包含重复账户"
    );
    let mut previous = Vec::new();
    let result = (|| -> Result<()> {
        for (name, value) in updates {
            let old = credentials.read(name)?;
            previous.push((name.as_str(), old));
            credentials.write(name, value)?;
        }
        for name in removals {
            let old = credentials.read(name)?;
            previous.push((name.as_str(), old));
            credentials.remove(name)?;
        }
        store.save()
    })();
    if let Err(error) = result {
        let mut rollback_failed = false;
        for (name, value) in previous.into_iter().rev() {
            let result = if let Some(value) = value {
                credentials.write(name, &value)
            } else {
                credentials.remove(name)
            };
            rollback_failed |= result.is_err();
        }
        if rollback_failed {
            return Err(error.context("保存失败；部分凭据回滚失败"));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_settings_preserve_legacy_defaults_and_validate_saved_identity_paths() -> Result<()> {
        let legacy = serde_json::json!({"id":Uuid::new_v4(), "name":"fixture", "group":"", "host":"localhost", "port":22, "user":"fixture", "auth":"Agent", "jump":null});
        let mut connection: Connection = serde_json::from_value(legacy)?;
        assert!(connection.ssh.agent_socket.is_none() && connection.ssh.agent_identity.is_none());
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)?;
        let directory = tempfile::tempdir()?;
        connection.ssh.agent_socket = Some(directory.path().join("socket"));
        connection.ssh.agent_identity = Some(
            key.public_key()
                .fingerprint(russh::keys::HashAlg::Sha256)
                .to_string(),
        );
        connection.validate()?;
        assert_eq!(
            serde_json::from_slice::<Connection>(&serde_json::to_vec(&connection)?)?,
            connection
        );
        let mut invalid = connection.clone();
        invalid.ssh.agent_socket = Some(PathBuf::from("relative/socket"));
        assert!(invalid.validate().is_err());
        for fingerprint in [
            "SHA256:",
            "SHA256:not-a-fingerprint",
            "SHA256:control\u{1b}",
        ] {
            invalid = connection.clone();
            invalid.ssh.agent_identity = Some(fingerprint.into());
            assert!(invalid.validate().is_err());
        }
        Ok(())
    }

    #[test]
    fn credential_removal_commits_with_profile_and_rolls_back_partial_failures() -> Result<()> {
        struct Fake {
            values: HashMap<String, String>,
            fail_remove: Option<String>,
        }
        impl Credentials for Fake {
            fn read(&mut self, name: &str) -> Result<Option<Zeroizing<String>>> {
                Ok(self.values.get(name).cloned().map(Zeroizing::new))
            }
            fn write(&mut self, name: &str, value: &str) -> Result<()> {
                self.values.insert(name.into(), value.into());
                Ok(())
            }
            fn remove(&mut self, name: &str) -> Result<()> {
                self.values.remove(name);
                if self.fail_remove.as_deref() == Some(name) {
                    self.fail_remove = None;
                    anyhow::bail!("simulated failure after deletion");
                }
                Ok(())
            }
        }
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path().to_path_buf())?;
        store.save()?;
        let original = fs::read(directory.path().join("profile.json"))?;
        store.profile.preferences.font_size = 17.;
        let previous = HashMap::from([
            ("removed".into(), "secret".into()),
            ("unrelated".into(), "keep".into()),
            ("partial".into(), "restore".into()),
        ]);
        let mut credentials = Fake {
            values: previous.clone(),
            fail_remove: Some("partial".into()),
        };
        assert!(
            save_transaction_changes(
                &store,
                &[],
                &["removed".into(), "partial".into()],
                &mut credentials
            )
            .is_err()
        );
        assert_eq!(
            credentials.values, previous,
            "a partial keychain deletion must restore every account"
        );
        assert_eq!(fs::read(directory.path().join("profile.json"))?, original);
        let invalid_store = Store {
            profile: store.profile.clone(),
            directory: directory.path().join("missing"),
        };
        assert!(
            save_transaction_changes(&invalid_store, &[], &["removed".into()], &mut credentials)
                .is_err()
        );
        assert_eq!(
            credentials.values, previous,
            "failed metadata commit must restore deleted credentials"
        );
        save_transaction_changes(
            &store,
            &[],
            &["removed".into(), "missing-account".into()],
            &mut credentials,
        )?;
        assert!(!credentials.values.contains_key("removed"));
        assert_eq!(credentials.values["unrelated"], "keep");
        assert_eq!(
            Store::open(directory.path().to_path_buf())?
                .profile
                .preferences
                .font_size,
            17.
        );
        assert!(
            save_transaction_changes(
                &store,
                &[("partial".into(), Zeroizing::new("new".into()))],
                &["partial".into()],
                &mut credentials
            )
            .is_err()
        );
        assert_eq!(
            credentials.values["partial"], "restore",
            "conflicting updates must fail before mutation"
        );
        Ok(())
    }

    #[test]
    fn deleted_credentials_reject_late_inflight_reads_and_clear_session_overrides() -> Result<()> {
        let cache = Arc::new(Mutex::new(CredentialCache::default()));
        let (started, wait_started) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let reading = cache.clone();
        let thread = std::thread::spawn(move || {
            cached_secret(&reading, "deleted", || {
                started.send(()).expect("signal read");
                wait_release.recv().expect("release read");
                Ok(Zeroizing::new("stale".into()))
            })
        });
        wait_started.recv_timeout(std::time::Duration::from_secs(2))?;
        cache.lock().expect("cache lock").remove("deleted");
        release.send(())?;
        assert_eq!(
            thread
                .join()
                .expect("reader thread")
                .expect_err("deleted read must fail")
                .downcast_ref::<CredentialError>(),
            Some(&CredentialError::Missing)
        );
        let mut state = cache.lock().expect("cache lock");
        assert!(state.value("deleted").is_none());
        state.insert("deleted", "saved");
        state
            .session
            .insert("deleted".into(), Zeroizing::new("override".into()));
        state
            .failures
            .insert("deleted".into(), CredentialError::Unavailable);
        state.remove("deleted");
        assert!(state.value("deleted").is_none() && !state.failures.contains_key("deleted"));
        Ok(())
    }

    #[test]
    fn denied_credentials_do_not_prompt_again_until_explicit_recovery() -> Result<()> {
        let cache = Mutex::new(CredentialCache::default());
        assert!(
            cached_secret(&cache, "provider-denied", || {
                Err(CredentialError::Unavailable.into())
            })
            .is_err()
        );
        let mut reads = 0;
        assert!(
            cached_secret(&cache, "provider-denied", || {
                reads += 1;
                Err(CredentialError::Unavailable.into())
            })
            .is_err()
        );
        assert_eq!(
            reads, 0,
            "another AI turn must not reopen a denied system prompt"
        );
        cached_secret(&cache, "other-provider", || {
            Ok(Zeroizing::new("other-key".into()))
        })?;
        cache.lock().expect("credential cache lock").retry();
        assert_eq!(
            cached_secret(&cache, "provider-denied", || {
                Ok(Zeroizing::new("authorized-key".into()))
            })?
            .as_str(),
            "authorized-key",
            "explicit retry can authorize storage again"
        );
        assert_eq!(
            cached_secret(&cache, "other-provider", || {
                Err(CredentialError::Unavailable.into())
            })?
            .as_str(),
            "other-key",
            "retry must retain previously authorized accounts"
        );
        Ok(())
    }

    #[test]
    fn simultaneous_requests_share_the_same_system_prompt() -> Result<()> {
        use std::{sync::mpsc, thread, time::Duration};
        for denied in [false, true] {
            let cache = Mutex::new(CredentialCache::default());
            thread::scope(|scope| -> Result<()> {
                let cache = &cache;
                let (started, start) = mpsc::channel();
                let (release, released) = mpsc::channel();
                let first = scope.spawn(move || {
                    cached_secret(cache, "shared-provider", || {
                        started.send(())?;
                        released.recv()?;
                        if denied {
                            Err(CredentialError::Unavailable.into())
                        } else {
                            Ok(Zeroizing::new("fixture-key".into()))
                        }
                    })
                });
                start.recv_timeout(Duration::from_secs(2))?;
                let (completed, completion) = mpsc::channel();
                let second = scope.spawn(move || {
                    let value = cached_secret(cache, "shared-provider", || {
                        Ok(Zeroizing::new("unexpected-second-read".into()))
                    });
                    completed
                        .send(())
                        .expect("completion receiver remains alive");
                    value
                });
                let premature = completion.recv_timeout(Duration::from_millis(100));
                release.send(())?;
                let first = first.join().expect("first credential reader did not panic");
                let second = second
                    .join()
                    .expect("second credential reader did not panic");
                assert!(
                    matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)),
                    "a concurrent pane must wait for the existing authorization prompt"
                );
                if denied {
                    assert!(
                        first.is_err() && second.is_err(),
                        "one denial applies to both requests"
                    );
                } else {
                    assert_eq!(first?.as_str(), "fixture-key");
                    assert_eq!(second?.as_str(), "fixture-key");
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn cached_credentials_survive_storage_lock_and_late_reads_cannot_replace_a_new_key()
    -> Result<()> {
        let cache = Mutex::new(CredentialCache::default());
        let first = cached_secret(&cache, "provider-a", || Ok(Zeroizing::new("first".into())))?;
        assert_eq!(first.as_str(), "first");
        let second = cached_secret(&cache, "provider-a", || {
            Err(CredentialError::Unavailable.into())
        })?;
        assert_eq!(
            second.as_str(),
            "first",
            "another terminal must use the validated session credential"
        );
        assert!(
            cached_secret(&cache, "provider-b", || Err(
                CredentialError::Unavailable.into()
            ))
            .is_err()
        );
        cache.lock().expect("credential cache lock").retry();
        let key = cached_secret(&cache, "provider-b", || {
            cache
                .lock()
                .expect("cache lock")
                .session
                .insert("provider-b".into(), Zeroizing::new("replacement".into()));
            Err(CredentialError::Unavailable.into())
        })?;
        assert_eq!(
            key.as_str(),
            "replacement",
            "a late keychain denial cannot override explicit recovery"
        );
        for number in 0..160 {
            cache
                .lock()
                .expect("cache lock")
                .insert(&format!("ssh-{number}"), "password");
        }
        assert_eq!(
            cached_secret(&cache, "provider-b", || Err(
                CredentialError::Unavailable.into()
            ))?
            .as_str(),
            "replacement",
            "evicting cached SSH passwords must retain explicit temporary provider keys"
        );
        cache.lock().expect("cache lock").values.clear();
        assert!(
            cached_secret(
                &cache,
                "provider-a",
                || Err(CredentialError::Missing.into())
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn shutdown_discards_late_storage_results_and_saved_keys_release_waiters() -> Result<()> {
        let cache = Mutex::new(CredentialCache::default());
        assert!(
            cached_secret(&cache, "provider-a", || {
                cache.lock().expect("credential cache lock").clear();
                Ok(Zeroizing::new("late-key".into()))
            })
            .is_err(),
            "shutdown cannot return or retain a late credential"
        );
        assert!(
            cached_secret(&cache, "provider-a", || {
                Err(CredentialError::Missing.into())
            })
            .is_err(),
            "a late key must not repopulate memory after shutdown"
        );
        cache.lock().expect("credential cache lock").retry();
        let value = cached_secret(&cache, "provider-a", || {
            cache
                .lock()
                .expect("credential cache lock")
                .insert("provider-a", "saved-key");
            Ok(Zeroizing::new("stale-key".into()))
        })?;
        assert_eq!(
            value.as_str(),
            "saved-key",
            "a completed save wins over old storage data"
        );
        Ok(())
    }

    #[test]
    fn credential_errors_do_not_expose_storage_payloads_or_masquerade_as_ssh_authentication() {
        let error = CredentialError::from(keyring::Error::BadEncoding(b"private-secret".to_vec()));
        assert_eq!(error, CredentialError::Unavailable);
        assert!(!format!("{error:?}: {error}").contains("private-secret"));
        assert_eq!(
            CredentialError::from(keyring::Error::NoEntry),
            CredentialError::Missing
        );
    }

    #[test]
    fn old_font_preferences_keep_terminal_settings_and_default_new_typography() -> Result<()> {
        let preferences: Preferences = serde_json::from_value(serde_json::json!({
            "font_family": "Custom Terminal", "font_size": 18.
        }))?;
        assert_eq!(preferences.font_family, "Custom Terminal");
        assert_eq!(preferences.font_size, 18.);
        assert_eq!(
            preferences.ui_font_family,
            crate::font_defaults::default_latin()
        );
        assert_eq!(
            preferences.font_family_zh,
            crate::font_defaults::default_chinese()
        );
        assert_eq!(
            preferences.ui_font_family_zh,
            crate::font_defaults::default_chinese()
        );
        assert_eq!(preferences.ui_font_size, 14.);
        Ok(())
    }

    #[test]
    fn terminal_preferences_merge_legacy_defaults_round_trip_and_reject_invalid_geometry()
    -> Result<()> {
        let legacy: Preferences =
            serde_json::from_value(serde_json::json!({"font_size": 18., "ui_font_size": 13.}))?;
        assert_eq!(legacy.terminal.padding_horizontal, 12.);
        assert_eq!(
            legacy.ui_font_size, 13.,
            "existing typography remains unchanged"
        );
        let mut profile = Profile::default();
        profile.preferences.terminal = TerminalOptions {
            line_height: 1.9,
            letter_spacing: 2.,
            padding_horizontal: 24.,
            padding_vertical: 16.,
            cursor_shape: CursorShape::Underline,
            cursor_blink: false,
            cursor_blink_ms: 800,
            cursor_width: 3.,
            unfocused_cursor: false,
            scroll_speed: 2.,
            copy_on_select: true,
            option_as_alt: false,
        };
        profile.validate()?;
        let original = serde_json::to_value(&profile)?;
        let restored: Profile = serde_json::from_value(original.clone())?;
        assert_eq!(
            serde_json::to_value(restored)?,
            original,
            "every terminal preference persists independently"
        );
        let mut invalid = profile.clone();
        invalid.preferences.terminal.letter_spacing = f32::NAN;
        assert!(invalid.validate().is_err());
        let mut invalid = profile.clone();
        invalid.preferences.terminal.padding_horizontal = -1.;
        assert!(invalid.validate().is_err());
        let mut invalid = profile.clone();
        invalid.preferences.terminal.line_height = 3.;
        assert!(invalid.validate().is_err());
        profile.preferences.terminal.cursor_blink_ms = 0;
        assert!(
            profile.validate().is_err(),
            "zero blink interval must not reach the renderer"
        );
        Ok(())
    }

    #[test]
    fn typography_round_trips_independently_and_invalid_names_are_rejected() -> Result<()> {
        let mut profile = Profile::default();
        profile.preferences.ui_font_family = "UI Latin".into();
        profile.preferences.ui_font_family_zh = "界面中文".into();
        profile.preferences.font_family = "Terminal Latin".into();
        profile.preferences.font_family_zh = "终端中文".into();
        profile.preferences.agent_font_family = "Agent Latin".into();
        profile.preferences.agent_font_family_zh = "助手中文".into();
        profile.preferences.agent_font_size = 12.;
        profile.preferences.ui_font_size = 16.;
        profile.preferences.font_size = 20.;
        profile.validate()?;
        let restored: Profile = serde_json::from_slice(&serde_json::to_vec(&profile)?)?;
        assert_eq!(restored.preferences.ui_font_family, "UI Latin");
        assert_eq!(restored.preferences.ui_font_family_zh, "界面中文");
        assert_eq!(restored.preferences.font_family, "Terminal Latin");
        assert_eq!(restored.preferences.font_family_zh, "终端中文");
        assert_eq!(restored.preferences.agent_font_family, "Agent Latin");
        assert_eq!(restored.preferences.agent_font_family_zh, "助手中文");
        assert_eq!(restored.preferences.agent_font_size, 12.);
        assert_eq!(restored.preferences.ui_font_size, 16.);
        assert_eq!(restored.preferences.font_size, 20.);
        profile.preferences.font_family_zh = "bad\0name".into();
        assert!(profile.validate().is_err());
        profile.preferences.font_family_zh = "Valid".into();
        profile.preferences.ui_font_size = f32::NAN;
        assert!(profile.validate().is_err());
        Ok(())
    }

    #[test]
    fn partial_credential_write_is_rolled_back_before_profile_publish() -> Result<()> {
        struct Partial(String);
        impl Credentials for Partial {
            fn read(&mut self, _: &str) -> Result<Option<Zeroizing<String>>> {
                Ok(Some(Zeroizing::new(self.0.clone())))
            }
            fn write(&mut self, _: &str, value: &str) -> Result<()> {
                self.0 = value.to_string();
                anyhow::ensure!(value != "new", "simulated failure after mutation");
                Ok(())
            }
            fn remove(&mut self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path().to_path_buf())?;
        let mut credentials = Partial("old".into());
        assert!(
            save_transaction(
                &store,
                &[("account".into(), Zeroizing::new("new".into()))],
                &mut credentials
            )
            .is_err()
        );
        assert_eq!(credentials.0, "old");
        assert!(!directory.path().join("profile.json").exists());
        Ok(())
    }

    #[test]
    fn saves_previous_profile_before_replacing_configuration() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path().to_path_buf())?;
        store.save()?;
        let original = fs::read(directory.path().join("profile.json"))?;
        store.profile.preferences.font_size = 20.;
        store.save()?;
        assert_eq!(
            fs::read(directory.path().join("profile.previous.json"))?,
            original
        );
        assert_eq!(
            Store::open(directory.path().to_path_buf())?
                .profile
                .preferences
                .font_size,
            20.
        );
        Ok(())
    }

    #[test]
    fn failed_profile_write_restores_previous_credentials_and_removes_new_accounts() -> Result<()> {
        struct Fake(std::collections::HashMap<String, String>);
        impl Credentials for Fake {
            fn read(&mut self, name: &str) -> Result<Option<Zeroizing<String>>> {
                Ok(self.0.get(name).cloned().map(Zeroizing::new))
            }
            fn write(&mut self, name: &str, value: &str) -> Result<()> {
                self.0.insert(name.into(), value.into());
                Ok(())
            }
            fn remove(&mut self, name: &str) -> Result<()> {
                self.0.remove(name);
                Ok(())
            }
        }
        let directory = tempfile::tempdir()?;
        let mut keys = Fake(std::collections::HashMap::from([(
            "existing".into(),
            "old".into(),
        )]));
        let store = Store {
            profile: Profile {
                schema: 1,
                ..Default::default()
            },
            directory: directory.path().join("missing-directory"),
        };
        assert!(
            save_transaction(
                &store,
                &[
                    ("existing".into(), Zeroizing::new("new".into())),
                    ("new-account".into(), Zeroizing::new("new".into()))
                ],
                &mut keys
            )
            .is_err()
        );
        assert_eq!(keys.0.get("existing").map(String::as_str), Some("old"));
        assert!(!keys.0.contains_key("new-account"));
        Ok(())
    }

    #[test]
    fn persists_metadata_without_credentials_and_rejects_corruption() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path().to_path_buf())?;
        store.profile.preferences.font_size = 16.;
        store.profile.connections.push(Connection {
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            id: Uuid::nil(),
            name: "Development".into(),
            group: "Work".into(),
            host: "dev.example".into(),
            port: 2222,
            user: "alice".into(),
            auth: Authentication::Password,
            jump: None,
            ssh: Default::default(),
            proxy: None,
        });
        store.save()?;
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("profile.json"))?)?;
        assert_eq!(json["connections"][0]["host"], "dev.example");
        assert_eq!(json["connections"][0]["port"], 2222);
        assert_eq!(json["preferences"]["font_size"], 16.);
        fs::write(directory.path().join("profile.json"), b"not json")?;
        assert!(
            Store::open(directory.path().to_path_buf()).is_err(),
            "corrupt configuration must remain visible"
        );
        Ok(())
    }
}
