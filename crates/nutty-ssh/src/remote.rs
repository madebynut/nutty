//! Registry-owned SSH connections, SFTP operations, and cancellable forwarding.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use async_channel::Sender;
use russh::{
    ChannelMsg, client,
    keys::{self, PrivateKeyWithHashAlg},
};
use russh_sftp::client::SftpSession;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, oneshot},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    config::{self, Authentication, Connection, Preferences},
    terminal::{Command, Terminal},
};

pub struct HostPrompt {
    /// Identifies the prompt the user saw, so a decision never applies to another host.
    pub id: Uuid,
    pub host: String,
    pub fingerprint: String,
    pub answer: oneshot::Sender<bool>,
}

pub struct AuthPrompt {
    pub host: String,
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<client::Prompt>,
    pub answer: oneshot::Sender<Option<Vec<String>>>,
}

struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    prompts: Sender<HostPrompt>,
    remote_routes: Arc<Mutex<HashMap<(String, u32), RemoteTarget>>>,
    remote_budget: Arc<tokio::sync::Semaphore>,
}

impl client::Handler for Client {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, key: &keys::PublicKeyOrCertificate) -> Result<bool> {
        let key = key.public_key();
        // A changed key is an error, never treated as a first-use approval.
        if verify_host_key(&self.host, self.port, &key, &self.known_hosts)? {
            return Ok(true);
        }
        ensure!(
            recordable_host(&self.host),
            "主机名包含 known_hosts 不支持的字符，无法记录服务器身份"
        );
        let (answer, response) = oneshot::channel();
        self.prompts
            .send(HostPrompt {
                id: Uuid::new_v4(),
                host: format!("{}:{}", self.host, self.port),
                fingerprint: key.fingerprint(keys::HashAlg::Sha256).to_string(),
                answer,
            })
            .await
            .context("主机身份确认窗口已关闭")?;
        if response.await.unwrap_or(false) {
            use std::io::Write as _;
            let mut options = std::fs::OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&self.known_hosts)?;
            let host = if self.port == 22 {
                self.host.clone()
            } else {
                format!("[{}]:{}", self.host, self.port)
            };
            writeln!(file, "{host} {}", key.to_openssh()?)?;
            file.sync_all()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        address: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<()> {
        let target = self
            .remote_routes
            .lock()
            .await
            .get(&(address.to_string(), port))
            .cloned();
        let Some(target) = target else {
            return Ok(());
        };
        let Ok(permit) = self.remote_budget.clone().try_acquire_owned() else {
            return Ok(());
        };
        tokio::spawn(async move {
            let _permit = permit;
            let forwarding = async {
                let mut socket = tokio::time::timeout(
                    Duration::from_secs(15),
                    TcpStream::connect((target.host.as_str(), target.port)),
                )
                .await??;
                reply.accept().await;
                let mut stream = channel.into_stream();
                tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
                Ok::<_, anyhow::Error>(())
            };
            tokio::select! {()=target.cancel.cancelled()=>{},_=forwarding=>{}}
        });
        Ok(())
    }

    /// Agent forwarding is never requested. Dropping `reply` rejects the channel
    /// (AdministrativelyProhibited) instead of russh's default accept.
    async fn server_channel_open_agent_forward(
        &mut self,
        _: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<()> {
        drop(reply);
        Ok(())
    }
}

#[derive(Clone)]
struct RemoteTarget {
    host: String,
    port: u16,
    cancel: CancellationToken,
}

pub struct Node {
    handle: RwLock<Arc<client::Handle<Client>>>,
    registry: Weak<Registry>,
    configuration: Arc<Vec<Connection>>,
    proxies: RwLock<Arc<Vec<crate::network::Proxy>>>,
    pub connection: Connection,
    pub cancel: CancellationToken,
    // A jump connection outlives all channels that use its stream.
    _parent: Option<Arc<Node>>,
    remote_routes: Arc<Mutex<HashMap<(String, u32), RemoteTarget>>>,
}

/// Keeps channel teardown effective even when a startup future is canceled by its caller.
struct SessionChannel {
    read: russh::ChannelReadHalf,
    write: Option<russh::ChannelWriteHalf<client::Msg>>,
}

impl SessionChannel {
    fn new(channel: russh::Channel<client::Msg>) -> Self {
        let (read, write) = channel.split();
        Self {
            read,
            write: Some(write),
        }
    }

    fn write(&self) -> &russh::ChannelWriteHalf<client::Msg> {
        self.write
            .as_ref()
            .expect("channel writer is present until drop")
    }
}

impl Drop for SessionChannel {
    fn drop(&mut self) {
        if let Some(write) = self.write.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), write.close()).await;
            });
        }
    }
}

pub struct Registry {
    // Sessions, transfers and forwards own transports; the registry only discovers them.
    nodes: Mutex<HashMap<Uuid, Weak<Node>>>,
    connecting: Mutex<HashMap<Uuid, Weak<Mutex<()>>>>,
    known_hosts: PathBuf,
    prompts: Sender<HostPrompt>,
    auth_prompts: Option<Sender<AuthPrompt>>,
}

/// Rolls back a remote listener reservation even if its startup future is dropped.
struct PendingRemoteForward {
    node: Option<Arc<Node>>,
    address: String,
    port: u32,
    cancel: CancellationToken,
}
impl Drop for PendingRemoteForward {
    fn drop(&mut self) {
        let Some(node) = self.node.take() else {
            return;
        };
        self.cancel.cancel();
        let address = self.address.clone();
        let port = self.port;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(30), async {
                    node.handle()
                        .await
                        .cancel_tcpip_forward(address.clone(), port)
                        .await
                })
                .await;
                node.remote_routes.lock().await.remove(&(address, port));
            });
        }
    }
}

impl Registry {
    pub fn new(directory: &Path, prompts: Sender<HostPrompt>) -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            known_hosts: directory.join("known_hosts"),
            prompts,
            auth_prompts: None,
        }
    }

    pub fn with_authentication(mut self, prompts: Sender<AuthPrompt>) -> Self {
        self.auth_prompts = Some(prompts);
        self
    }

    #[cfg(test)]
    pub async fn connect(
        self: &Arc<Self>,
        target: Uuid,
        connections: &[Connection],
    ) -> Result<Arc<Node>> {
        self.connect_using(target, connections, &[]).await
    }
    pub async fn connect_using(
        self: &Arc<Self>,
        target: Uuid,
        connections: &[Connection],
        proxies: &[crate::network::Proxy],
    ) -> Result<Arc<Node>> {
        self.connect_credentials(target, connections, proxies, &HashMap::new())
            .await
    }

    async fn connect_credentials(
        self: &Arc<Self>,
        target: Uuid,
        connections: &[Connection],
        proxies: &[crate::network::Proxy],
        credentials: &HashMap<Uuid, zeroize::Zeroizing<String>>,
    ) -> Result<Arc<Node>> {
        let route = route(target, connections)?;
        let configuration: Arc<Vec<Connection>> = Arc::new(
            route
                .iter()
                .map(|connection| (*connection).clone())
                .collect(),
        );
        let proxies = Arc::new(proxies.to_vec());
        let mut parent: Option<Arc<Node>> = None;
        for connection in route {
            let gate = {
                let mut gates = self.connecting.lock().await;
                gates.retain(|_, gate| gate.strong_count() > 0);
                if let Some(gate) = gates.get(&connection.id).and_then(Weak::upgrade) {
                    gate
                } else {
                    let gate = Arc::new(Mutex::new(()));
                    gates.insert(connection.id, Arc::downgrade(&gate));
                    gate
                }
            };
            let _connection_guard = gate.lock().await;
            let cached = {
                let mut nodes = self.nodes.lock().await;
                nodes.retain(|_, node| node.strong_count() > 0);
                nodes.get(&connection.id).and_then(Weak::upgrade)
            };
            if let Some(node) = &cached
                && !node.handle().await.is_closed()
                && !node.cancel.is_cancelled()
                && node.connection == *connection
            {
                *node.proxies.write().await = proxies.clone();
                parent = Some(node.clone());
                continue;
            }
            let reusable =
                cached.filter(|node| !node.cancel.is_cancelled() && node.connection == *connection);
            if reusable.is_none() {
                self.disconnect(connection.id).await?;
            }
            let remote_routes = reusable
                .as_ref()
                .map(|node| node.remote_routes.clone())
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new())));
            let client = Client {
                host: connection.host.clone(),
                port: connection.port,
                known_hosts: self.known_hosts.clone(),
                prompts: self.prompts.clone(),
                remote_routes: remote_routes.clone(),
                remote_budget: Arc::new(tokio::sync::Semaphore::new(128)),
            };
            let config = Arc::new(client::Config {
                keepalive_interval: (connection.ssh.keepalive_seconds > 0)
                    .then(|| Duration::from_secs(connection.ssh.keepalive_seconds)),
                keepalive_max: connection.ssh.keepalive_max,
                preferred: connection.ssh.algorithms.preferred()?,
                ..Default::default()
            });
            let establishing = async {
                if let Some(parent) = &parent {
                    let channel = parent
                        .handle()
                        .await
                        .channel_open_direct_tcpip(
                            &connection.host,
                            u32::from(connection.port),
                            "127.0.0.1",
                            0,
                        )
                        .await?;
                    client::connect_stream(config, channel.into_stream(), client).await
                } else {
                    let proxy = connection
                        .proxy
                        .map(|id| {
                            proxies
                                .iter()
                                .find(|proxy| proxy.id == id)
                                .context("SSH 代理不存在")
                        })
                        .transpose()?;
                    let stream =
                        crate::network::connect(&connection.host, connection.port, proxy).await?;
                    client::connect_stream(config, stream, client).await
                }
            };
            let mut handle = tokio::time::timeout(
                Duration::from_secs(connection.ssh.connect_timeout_seconds),
                establishing,
            )
            .await
            .context("SSH 连接超时")?
            .with_context(|| {
                format!(
                    "连接 {}@{}:{}",
                    connection.user, connection.host, connection.port
                )
            })?;
            let authenticating = async {
                let authenticated = match &connection.auth {
                    Authentication::Interactive => {
                        let prompts = self.auth_prompts.as_ref().context("交互式认证界面不可用")?;
                        authenticate_interactive(&mut handle, connection, prompts).await?
                    }
                    Authentication::Password => {
                        let id = connection.id.to_string();
                        let password = if let Some(value) = credentials.get(&connection.id) {
                            value.clone()
                        } else {
                            tokio::task::spawn_blocking(move || config::secret(&id)).await??
                        };
                        handle
                            .authenticate_password(&connection.user, password.as_str())
                            .await?
                            .success()
                    }
                    Authentication::Key { path, certificate } => {
                        let path = path.clone();
                        let id = connection.id.to_string();
                        let override_secret = credentials.get(&connection.id).cloned();
                        let key = tokio::task::spawn_blocking(move || -> Result<_> {
                            if let Ok(key) = keys::load_secret_key(&path, None) {
                                return Ok(key);
                            }
                            let passphrase = match override_secret {
                                Some(secret) => secret,
                                None => config::secret(&id).context("无法读取私钥口令")?,
                            };
                            Ok(keys::load_secret_key(path, Some(passphrase.as_str()))?)
                        })
                        .await??;
                        if let Some(path) = certificate {
                            let certificate = keys::load_openssh_certificate(path)?;
                            handle
                                .authenticate_openssh_cert(
                                    &connection.user,
                                    Arc::new(key),
                                    certificate,
                                )
                                .await?
                                .success()
                        } else {
                            let hash = handle.best_supported_rsa_hash().await?.flatten();
                            handle
                                .authenticate_publickey(
                                    &connection.user,
                                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                                )
                                .await?
                                .success()
                        }
                    }
                    Authentication::Agent => {
                        #[cfg(any(unix, windows))]
                        {
                            let (mut agent, _) =
                                crate::ssh_agent::connect(connection.ssh.agent_socket.as_deref())
                                    .await?;
                            let identities = tokio::time::timeout(
                                Duration::from_secs(3),
                                agent.request_identities(),
                            )
                            .await
                            .context("SSH Agent 获取密钥超时")??;
                            ensure!(!identities.is_empty(), "SSH Agent 中没有密钥，请先加载密钥");
                            ensure!(
                                identities.len() <= 128,
                                "SSH Agent 密钥数量超过 128，请选择专用 Agent"
                            );
                            let mut success = false;
                            let mut matched = false;
                            for identity in identities {
                                let fingerprint = identity
                                    .public_key()
                                    .fingerprint(keys::HashAlg::Sha256)
                                    .to_string();
                                if connection
                                    .ssh
                                    .agent_identity
                                    .as_ref()
                                    .is_some_and(|selected| selected != &fingerprint)
                                {
                                    continue;
                                }
                                matched = true;
                                let hash = handle.best_supported_rsa_hash().await?.flatten();
                                let result = match identity {
                                    keys::agent::AgentIdentity::PublicKey { key, .. } => {
                                        handle
                                            .authenticate_publickey_with(
                                                &connection.user,
                                                key,
                                                hash,
                                                &mut agent,
                                            )
                                            .await
                                    }
                                    keys::agent::AgentIdentity::Certificate {
                                        certificate, ..
                                    } => {
                                        handle
                                            .authenticate_certificate_with(
                                                &connection.user,
                                                certificate,
                                                hash,
                                                &mut agent,
                                            )
                                            .await
                                    }
                                };
                                if result
                                    .context("SSH Agent 签名失败，请解锁 Agent 或允许此次签名")?
                                    .success()
                                {
                                    success = true;
                                    break;
                                }
                            }
                            ensure!(
                                matched,
                                "配置的 SSH Agent 密钥未加载，请重新检测或选择其他密钥"
                            );
                            ensure!(
                                success,
                                "服务器拒绝 SSH Agent 密钥，请确认用户名及服务器授权密钥"
                            );
                            success
                        }
                        #[cfg(not(any(unix, windows)))]
                        {
                            bail!("此平台的 SSH Agent 尚未接入，请使用密钥或密码")
                        }
                    }
                };
                Ok::<_, anyhow::Error>(authenticated)
            };
            let authenticated = tokio::time::timeout(Duration::from_secs(120), authenticating)
                .await
                .context("SSH 身份认证超时")??;
            ensure!(authenticated, "SSH 身份认证失败");
            let handle = Arc::new(handle);
            let node = if let Some(node) = reusable {
                let routes = node.remote_routes.lock().await.clone();
                for ((address, port), target) in routes {
                    if !target.cancel.is_cancelled() {
                        handle
                            .tcpip_forward(address, port)
                            .await
                            .context("恢复远程端口转发失败")?;
                    }
                }
                *node.handle.write().await = handle;
                *node.proxies.write().await = proxies.clone();
                node
            } else {
                Arc::new(Node {
                    handle: RwLock::new(handle),
                    registry: Arc::downgrade(self),
                    configuration: configuration.clone(),
                    proxies: RwLock::new(proxies.clone()),
                    connection: connection.clone(),
                    cancel: CancellationToken::new(),
                    _parent: parent,
                    remote_routes,
                })
            };
            self.nodes
                .lock()
                .await
                .insert(connection.id, Arc::downgrade(&node));
            parent = Some(node);
        }
        parent.context("SSH 路由为空")
    }

    /// Probes authentication along the full route without opening terminal channels or saving secrets.
    pub async fn probe(
        &self,
        target: Uuid,
        connections: &[Connection],
        proxies: &[crate::network::Proxy],
        password: zeroize::Zeroizing<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let isolated = Arc::new(Self {
            nodes: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashMap::new()),
            known_hosts: self.known_hosts.clone(),
            prompts: self.prompts.clone(),
            auth_prompts: self.auth_prompts.clone(),
        });
        let mut credentials = HashMap::new();
        if !password.is_empty() {
            credentials.insert(target, password);
        }
        let result = tokio::select! {
            () = cancel.cancelled() => Err(anyhow::anyhow!("连接测试已取消")),
            result = async {
                let node = isolated.connect_credentials(target, connections, proxies, &credentials).await?;
                if node.connection.kind == config::ConnectionKind::Sftp {
                    node.list(&node.connection.initial_directory).await?;
                }
                Ok(())
            } => result,
        };
        let ids: Vec<_> = isolated.nodes.lock().await.keys().copied().collect();
        for id in ids {
            let _ = isolated.disconnect(id).await;
        }
        result
    }

    pub async fn refresh_proxies(&self, proxies: Vec<crate::network::Proxy>) {
        let proxies = Arc::new(proxies);
        let nodes: Vec<_> = self
            .nodes
            .lock()
            .await
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for node in nodes {
            *node.proxies.write().await = proxies.clone();
        }
    }

    pub async fn disconnect(&self, id: Uuid) -> Result<()> {
        let disconnected = {
            let mut nodes = self.nodes.lock().await;
            let mut disconnected = Vec::new();
            nodes.retain(|_, node| {
                let Some(node) = node.upgrade() else {
                    return false;
                };
                let mut ancestor: Option<&Arc<Node>> = Some(&node);
                while let Some(current) = ancestor {
                    if current.connection.id == id {
                        disconnected.push(node.clone());
                        return false;
                    }
                    ancestor = current._parent.as_ref();
                }
                true
            });
            disconnected
        };
        for node in &disconnected {
            node.cancel.cancel();
        }
        for node in disconnected {
            node.cancel.cancel();
            node.handle()
                .await
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await?;
        }
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn retryable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            )
        }) || cause.is::<tokio::time::error::Elapsed>()
            || cause.downcast_ref::<russh::Error>().is_some_and(|error| {
                matches!(
                    error,
                    russh::Error::Disconnect
                        | russh::Error::RecvError
                        | russh::Error::HUP
                        | russh::Error::ConnectionTimeout
                        | russh::Error::KeepaliveTimeout
                )
            })
    })
}

async fn authenticate_interactive(
    handle: &mut client::Handle<Client>,
    connection: &Connection,
    sender: &Sender<AuthPrompt>,
) -> Result<bool> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(&connection.user, None)
        .await?;
    for round in 0..8 {
        match response {
            client::KeyboardInteractiveAuthResponse::Success => return Ok(true),
            client::KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                if round == 0
                    && !remaining_methods.contains(&russh::MethodKind::KeyboardInteractive)
                {
                    bail!("服务器不支持交互式认证，请选择服务器支持的认证方式");
                }
                bail!("服务器拒绝交互式认证，请检查密码或验证码");
            }
            client::KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                ensure!(
                    prompts.len() <= 8
                        && name.len()
                            + instructions.len()
                            + prompts
                                .iter()
                                .map(|prompt| prompt.prompt.len())
                                .sum::<usize>()
                            <= 16 * 1024,
                    "服务器认证提示过多或过长"
                );
                let count = prompts.len();
                if count == 0 {
                    response = handle
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await?;
                    continue;
                }
                let (answer, receive) = oneshot::channel();
                sender
                    .send(AuthPrompt {
                        host: format!(
                            "{}@{}:{}",
                            connection.user, connection.host, connection.port
                        ),
                        name,
                        instructions,
                        prompts,
                        answer,
                    })
                    .await
                    .context("认证窗口已关闭")?;
                let answers = tokio::time::timeout(Duration::from_secs(120), receive)
                    .await
                    .context("认证等待超时")?
                    .context("认证已取消")?
                    .context("认证已取消")?;
                ensure!(answers.len() == count, "认证回答数量不匹配");
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
    bail!("服务器认证轮次超出限制")
}

/// known_hosts treats these characters as separators or patterns. Trusting such a name
/// would record trust for other hosts, or an entry that never matches again.
fn recordable_host(host: &str) -> bool {
    !host.is_empty()
        && !host.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, ',' | '#' | '|' | '*' | '?' | '!' | '[' | ']')
        })
}

fn verify_host_key(host: &str, port: u16, key: &keys::PublicKey, path: &Path) -> Result<bool> {
    match std::fs::read(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法读取已知主机文件"),
    }
    let known = keys::known_hosts::known_host_keys_path(host, port, path)?;
    if known.is_empty() {
        return Ok(false);
    }
    ensure!(
        known.iter().any(|(_, expected)| expected == key),
        "服务器密钥与已保存的身份不一致"
    );
    Ok(true)
}

pub fn route(target: Uuid, connections: &[Connection]) -> Result<Vec<&Connection>> {
    let mut seen = HashSet::new();
    let mut route = Vec::new();
    let mut next = Some(target);
    while let Some(id) = next {
        ensure!(seen.insert(id), "跳板连接存在循环");
        ensure!(route.len() < 16, "SSH 路由最多允许 16 跳");
        let connection = connections
            .iter()
            .find(|connection| connection.id == id)
            .context("跳板连接不存在")?;
        connection.validate()?;
        next = connection.jump;
        route.push(connection);
    }
    route.reverse();
    Ok(route)
}

pub enum FileOperation {
    Directory(String),
    Rename { source: String, target: String },
    Remove { path: String, directory: bool },
    Permissions { path: String, mode: u32 },
}

impl Node {
    async fn handle(&self) -> Arc<client::Handle<Client>> {
        self.handle.read().await.clone()
    }
    pub async fn forward_remote(
        self: &Arc<Self>,
        bind: SocketAddr,
        target: (String, u16),
    ) -> Result<Forward> {
        ensure!(bind.port() > 0, "远程监听端口必须大于零");
        let address = bind.ip().to_string();
        let port = u32::from(bind.port());
        let cancel = self.cancel.child_token();
        {
            let mut routes = self.remote_routes.lock().await;
            ensure!(
                !routes.contains_key(&(address.clone(), port)),
                "此远程端口已在转发"
            );
            routes.insert(
                (address.clone(), port),
                RemoteTarget {
                    host: target.0,
                    port: target.1,
                    cancel: cancel.clone(),
                },
            );
        }
        let mut pending = PendingRemoteForward {
            node: Some(self.clone()),
            address: address.clone(),
            port,
            cancel: cancel.clone(),
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            self.handle()
                .await
                .tcpip_forward(address.clone(), port)
                .await
        })
        .await
        .context("远程转发建立超时")??;
        let node = self.clone();
        let stopping = cancel.clone();
        let task = tokio::spawn(async move {
            stopping.cancelled().await;
            if !matches!(
                tokio::time::timeout(Duration::from_secs(30), async {
                    node.handle()
                        .await
                        .cancel_tcpip_forward(address.clone(), port)
                        .await
                })
                .await,
                Ok(Ok(_))
            ) && !node.cancel.is_cancelled()
            {
                tracing::warn!("remote forwarding cancellation was rejected");
            }
            node.remote_routes.lock().await.remove(&(address, port));
        });
        pending.node = None;
        Ok(Forward {
            address: bind,
            cancel,
            _task: task,
        })
    }

    async fn terminal_channel(
        &self,
        size: crate::terminal::TermSize,
    ) -> Result<(SessionChannel, Arc<client::Handle<Client>>, Vec<u8>)> {
        let transport = self.handle().await;
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.connection.ssh.connect_timeout_seconds);
        let channel = tokio::select! {
            biased;
            () = self.cancel.cancelled() => bail!("SSH 连接已取消"),
            result = tokio::time::timeout_at(deadline, transport.channel_open_session()) =>
                result.context("SSH 会话通道建立超时")??,
        };
        let mut channel = SessionChannel::new(channel);
        let mut output = Vec::new();
        let start = async {
            channel
                .write()
                .request_pty(
                    true,
                    "xterm-256color",
                    u32::from(size.columns),
                    u32::from(size.rows),
                    0,
                    0,
                    &[],
                )
                .await?;
            Self::startup_reply(&mut channel, &mut output, "PTY").await?;
            channel.write().request_shell(true).await?;
            Self::startup_reply(&mut channel, &mut output, "Shell").await
        };
        let result = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(anyhow::anyhow!("SSH 连接已取消")),
            result = tokio::time::timeout_at(deadline, start) =>
                result.context("SSH PTY / Shell 启动超时").and_then(|result| result),
        };
        result?;
        Ok((channel, transport, output))
    }

    async fn startup_reply(
        channel: &mut SessionChannel,
        output: &mut Vec<u8>,
        phase: &str,
    ) -> Result<()> {
        loop {
            match channel.read.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => bail!("服务器拒绝 SSH {phase} 请求"),
                Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                    ensure!(
                        output.len() + data.len() <= 64 * 1024,
                        "SSH 启动输出超过 64 KiB"
                    );
                    output.extend_from_slice(&data);
                }
                Some(
                    ChannelMsg::Eof
                    | ChannelMsg::Close
                    | ChannelMsg::ExitStatus { .. }
                    | ChannelMsg::ExitSignal { .. },
                )
                | None => bail!("SSH {phase} 启动前通道已关闭"),
                _ => {}
            }
        }
    }
    async fn terminal_environment(&self) -> crate::session_environment::Environment {
        self.probe_environment()
            .await
            .unwrap_or_else(crate::session_environment::Environment::unknown_ssh)
    }

    async fn probe_environment(&self) -> Option<crate::session_environment::Environment> {
        let channel = tokio::time::timeout(
            Duration::from_millis(500),
            self.handle().await.channel_open_session(),
        )
        .await
        .ok()?
        .ok()?;
        let mut channel = SessionChannel::new(channel);
        let probe = async {
            channel
                .write()
                .exec(true, crate::session_environment::SSH_PROBE)
                .await
                .ok()?;
            let mut bytes = Vec::new();
            while let Some(message) = channel.read.wait().await {
                match message {
                    ChannelMsg::Data { data } if bytes.len() + data.len() <= 1024 => {
                        bytes.extend_from_slice(&data);
                        if crate::session_environment::Environment::from_ssh_probe(&bytes).is_some()
                        {
                            break;
                        }
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    ChannelMsg::Failure | ChannelMsg::Data { .. } => return None,
                    _ => {}
                }
            }
            crate::session_environment::Environment::from_ssh_probe(&bytes)
        };
        let result = tokio::select! {
            () = self.cancel.cancelled() => None,
            result = tokio::time::timeout(Duration::from_millis(500), probe) => result.ok().flatten(),
        };
        result
    }
    fn shell_hooks(environment: &crate::session_environment::Environment) -> Option<String> {
        let shell = Path::new(environment.startup_shell.as_deref()?)
            .file_name()?
            .to_str()?;
        crate::shell_integration::remote_command(shell)
    }
    pub async fn terminal(self: &Arc<Self>, preferences: &Preferences) -> Result<Terminal> {
        let mut size = crate::terminal::TermSize::default();
        let environment = self.terminal_environment().await;
        let hooks = Self::shell_hooks(&environment);
        let (mut channel, mut transport, output) = self.terminal_channel(size).await?;
        let (terminal, input) = Terminal::channel(preferences, self.connection.name.clone());
        terminal.engine.set_environment(environment);
        terminal.engine.feed(&output);
        if let Some(hooks) = hooks {
            let wire = terminal.engine.remote_integration_started(&hooks);
            channel.write().data(wire.as_bytes()).await?;
        }
        terminal
            .engine
            .set_remote_identity(&self.connection.user, &self.connection.host);
        let engine = terminal.engine.clone();
        let node = self.clone();
        tokio::spawn(async move {
            let mut integration_tick = tokio::time::interval(Duration::from_secs(1));
            integration_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            'session: loop {
                let mut normal_exit = false;
                let result=async {
                    loop {
                        tokio::select! {
                            biased;
                            ()=node.cancel.cancelled()=>{normal_exit=true;break;},
                            ()=engine.wait_input_closed()=>{normal_exit=true;break;},
                            _=integration_tick.tick(), if engine.remote_integration_pending()=>engine.request_directory_tracking(),
                            command=input.recv()=>match command {
                                Ok(Command::Input(bytes))=>tokio::select! {
                                    biased;
                                    ()=node.cancel.cancelled()=>{normal_exit=true;break;},
                                    ()=engine.wait_input_closed()=>{normal_exit=true;break;},
                                    result=channel.write().data(bytes.as_slice())=>result?,
                                },
                                Ok(Command::Resize(next))=>{size=next;channel.write().window_change(u32::from(size.columns),u32::from(size.rows),0,0).await?;},
                                Err(_)=>{normal_exit=true;break;},
                            },
                            message=channel.read.wait()=>match message {
                                Some(ChannelMsg::Data{data})|Some(ChannelMsg::ExtendedData{data,..})=>{engine.feed(&data);engine.request_directory_tracking();},
                                Some(ChannelMsg::ExitStatus{..}|ChannelMsg::ExitSignal{..})=>{normal_exit=true;break;},
                                Some(ChannelMsg::Eof|ChannelMsg::Close)|None=>break,
                                _=>{},
                            }
                        }
                    }
                    Ok::<_,anyhow::Error>(())
                }.await;
                if normal_exit || node.cancel.is_cancelled() {
                    break;
                }
                // The channel may close a few scheduler ticks before its transport state.
                for _ in 0..5 {
                    if transport.is_closed() {
                        break;
                    }
                    tokio::select! {
                        () = node.cancel.cancelled() => break 'session,
                        () = engine.wait_input_closed() => break 'session,
                        () = tokio::time::sleep(Duration::from_millis(20)) => {},
                    }
                }
                let disconnected = transport.is_closed();
                if !disconnected {
                    if result.is_err() {
                        engine.fail("SSH 通道已中断".into());
                    }
                    break;
                }
                if !node.connection.ssh.reconnect || node.connection.ssh.reconnect_attempts == 0 {
                    engine.fail("SSH 连接已中断，可手动重连".into());
                    break;
                }
                engine.pause();
                while let Ok(command) = input.try_recv() {
                    if let Command::Resize(next) = command {
                        size = next;
                    }
                }
                let Some(registry) = node.registry.upgrade() else {
                    break;
                };
                for attempt in 0..node.connection.ssh.reconnect_attempts {
                    engine.fail(format!(
                        "SSH 连接中断，正在重连 {}/{}",
                        attempt + 1,
                        node.connection.ssh.reconnect_attempts
                    ));
                    let reconnect = async {
                        tokio::time::sleep(Duration::from_millis(500 * (1_u64 << attempt.min(4))))
                            .await;
                        let proxies = node.proxies.read().await.clone();
                        registry
                            .connect_using(node.connection.id, &node.configuration, &proxies)
                            .await?;
                        let environment = node.terminal_environment().await;
                        let next_hooks = Self::shell_hooks(&environment);
                        let channel = node.terminal_channel(size).await?;
                        Ok::<_, anyhow::Error>((channel, environment, next_hooks))
                    };
                    let result = tokio::select! {()=node.cancel.cancelled()=>break 'session,()=engine.wait_input_closed()=>break 'session,result=reconnect=>result};
                    match result {
                        Ok(((next, handle, output), environment, hooks)) => {
                            channel = next;
                            transport = handle;
                            engine.set_environment(environment);
                            engine
                                .set_remote_identity(&node.connection.user, &node.connection.host);
                            engine.resume();
                            engine.feed(&output);
                            if let Some(hooks) = hooks {
                                let wire = engine.remote_integration_started(&hooks);
                                if let Err(error) = channel.write().data(wire.as_bytes()).await {
                                    engine.fail(format!("SSH Shell 初始化失败：{error:#}"));
                                    break 'session;
                                }
                            }
                            continue 'session;
                        }
                        Err(error) => {
                            engine.fail(format!("SSH 重连失败：{error:#}"));
                            if !retryable(&error) {
                                break 'session;
                            }
                        }
                    }
                }
                break;
            }
            engine.finish();
        });
        Ok(terminal)
    }

    async fn file_transport(&self) -> Result<Arc<client::Handle<Client>>> {
        let handle = self.handle().await;
        if handle.is_closed() && self.connection.ssh.reconnect && !self.cancel.is_cancelled() {
            let registry = self.registry.upgrade().context("连接管理器已结束")?;
            let proxies = self.proxies.read().await.clone();
            Box::pin(registry.connect_using(self.connection.id, &self.configuration, &proxies))
                .await?;
            return Ok(self.handle().await);
        }
        Ok(handle)
    }
    pub(crate) async fn transfer_session(&self) -> Result<russh_sftp::client::RawSftpSession> {
        let open = async {
            let channel = self.file_transport().await?.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let raw = russh_sftp::client::RawSftpSession::new_with_config(
                channel.into_stream(),
                russh_sftp::client::Config {
                    request_timeout_secs: 30,
                    max_packet_len: 128 * 1024,
                    max_concurrent_reads: 1,
                    max_concurrent_writes: 1,
                    ..Default::default()
                },
            );
            raw.init().await?;
            Ok(raw)
        };
        tokio::select! {
            () = self.cancel.cancelled() => bail!("连接已断开"),
            result = tokio::time::timeout(Duration::from_secs(30), open) => result.context("SFTP 初始化超时")?,
        }
    }

    pub async fn sftp(&self) -> Result<SftpSession> {
        let timeout = self.connection.ssh.connect_timeout_seconds;
        let open = async {
            let channel = self.file_transport().await?.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let session = SftpSession::new(channel.into_stream()).await?;
            session.set_timeout(timeout);
            Ok(session)
        };
        tokio::select! {
            () = self.cancel.cancelled() => anyhow::bail!("连接已断开"),
            result = tokio::time::timeout(Duration::from_secs(timeout), open) => result.context("SFTP 初始化超时")?,
        }
    }

    pub async fn file_operation(&self, operation: FileOperation) -> Result<()> {
        let action = async {
            let sftp = self.sftp().await?;
            match operation {
                FileOperation::Directory(path) => sftp.create_dir(path).await?,
                FileOperation::Rename { source, target } => {
                    ensure!(!sftp.try_exists(&target).await?, "目标文件已存在");
                    sftp.rename(source, target).await?;
                }
                FileOperation::Remove { path, directory } => {
                    if directory {
                        sftp.remove_dir(path).await?;
                    } else {
                        sftp.remove_file(path).await?;
                    }
                }
                FileOperation::Permissions { path, mode } => {
                    ensure!(mode <= 0o7777, "文件权限无效");
                    let mut metadata = sftp.metadata(&path).await?;
                    metadata.permissions = Some(mode);
                    sftp.set_metadata(path, metadata).await?;
                }
            }
            Ok(())
        };
        tokio::select! {
            () = self.cancel.cancelled() => anyhow::bail!("连接已断开"),
            result = tokio::time::timeout(Duration::from_secs(self.connection.ssh.connect_timeout_seconds), action) => result.context("文件操作超时")?,
        }
    }
    /// Verifies directory access without retaining a potentially large file listing.
    pub async fn verify_directory(&self, path: &str) -> Result<()> {
        let raw = self.transfer_session().await?;
        let handle = raw.opendir(path).await?.handle;
        raw.close(handle).await?;
        Ok(())
    }
    pub async fn list(&self, path: &str) -> Result<Vec<RemoteFile>> {
        let read = async {
            let channel = self.file_transport().await?.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let raw = russh_sftp::client::RawSftpSession::new_with_config(
                channel.into_stream(),
                russh_sftp::client::Config {
                    request_timeout_secs: self.connection.ssh.connect_timeout_seconds,
                    max_packet_len: 128 * 1024,
                    ..Default::default()
                },
            );
            raw.init().await?;
            let handle = raw.opendir(path).await?.handle;
            let mut files = Vec::new();
            let mut name_bytes = 0;
            loop {
                match raw.readdir(handle.as_str()).await {
                    Ok(page) => {
                        ensure!(
                            files.len() + page.files.len() <= 100_000,
                            "远程目录超过 100000 项，请选择子目录"
                        );
                        for entry in page.files {
                            let name = entry.filename;
                            name_bytes += name.len();
                            ensure!(
                                name_bytes <= 32 * 1024 * 1024,
                                "目录名称数据超过 32 MiB 限制"
                            );
                            if matches!(name.as_str(), "." | "..") || name.contains(['/', '\0']) {
                                continue;
                            }
                            files.push(RemoteFile {
                                name,
                                directory: entry.attrs.is_dir(),
                                size: entry.attrs.size.unwrap_or(0),
                            });
                        }
                    }
                    Err(russh_sftp::client::error::Error::Status(status))
                        if status.status_code == russh_sftp::protocol::StatusCode::Eof =>
                    {
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            raw.close(handle).await?;
            files.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(files)
        };
        tokio::select! {
            () = self.cancel.cancelled() => anyhow::bail!("连接已断开"),
            result = tokio::time::timeout(Duration::from_secs(self.connection.ssh.connect_timeout_seconds), read) => result.context("读取目录超时")?,
        }
    }

    pub async fn read_text(&self, path: &str) -> Result<String> {
        let sftp = self.sftp().await?;
        let mut bytes = Vec::new();
        sftp.open(path)
            .await?
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= 2 * 1024 * 1024,
            "预览只支持 2 MiB 以内的文本文件"
        );
        String::from_utf8(bytes).context("文件不是 UTF-8 文本")
    }

    pub async fn save_text(&self, path: &str, original: &str, replacement: &str) -> Result<()> {
        ensure!(!self.cancel.is_cancelled(), "连接已断开");
        let channel = self.file_transport().await?.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        crate::sftp_edit::save(
            channel.into_stream(),
            path,
            original,
            replacement,
            &self.cancel,
        )
        .await
    }

    pub async fn download(
        &self,
        remote: &str,
        local: &Path,
        cancel: &CancellationToken,
        progress: &Sender<u64>,
        limit: Option<std::num::NonZeroU64>,
    ) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "传输已取消");
        ensure!(!local.exists(), "目标文件已存在，请选择新的文件名");
        let temporary = local.with_file_name(format!(".nutty-{}.part", Uuid::new_v4()));
        let result = async {
            let sftp = self.sftp().await?;
            let mut source = sftp.open(remote).await?;
            let mut output = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .await?;
            copy(&mut source, &mut output, cancel, progress, limit).await?;
            output.sync_all().await?;
            drop(output);
            ensure!(!cancel.is_cancelled(), "传输已取消");
            // Atomic no-clobber publish, including filesystems without hard links.
            let (from, to) = (temporary.clone(), local.to_path_buf());
            tokio::task::spawn_blocking(move || nutty_core::files::publish_new(&from, &to))
                .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }

    pub async fn upload(
        &self,
        local: &Path,
        remote: &str,
        cancel: &CancellationToken,
        progress: &Sender<u64>,
        limit: Option<std::num::NonZeroU64>,
    ) -> Result<()> {
        ensure!(!cancel.is_cancelled(), "传输已取消");
        let sftp = self.sftp().await?;
        ensure!(
            !sftp.try_exists(remote).await?,
            "远程目标文件已存在，请选择新的文件名"
        );
        let temporary = format!("{remote}.nutty-{}.part", Uuid::new_v4());
        let result = async {
            let mut source = tokio::fs::File::open(local).await?;
            let flags = russh_sftp::protocol::OpenFlags::CREATE
                | russh_sftp::protocol::OpenFlags::EXCLUDE
                | russh_sftp::protocol::OpenFlags::WRITE;
            let mut output = sftp.open_with_flags(&temporary, flags).await?;
            copy(&mut source, &mut output, cancel, progress, limit).await?;
            output.shutdown().await?;
            ensure!(!cancel.is_cancelled(), "传输已取消");
            sftp.rename(&temporary, remote).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = sftp.remove_file(&temporary).await;
        }
        result
    }

    pub async fn forward(
        self: &Arc<Self>,
        bind: SocketAddr,
        target: Option<(String, u16)>,
    ) -> Result<Forward> {
        let listener = TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        let cancel = self.cancel.child_token();
        let task_cancel = cancel.clone();
        let node = self.clone();
        let task = tokio::spawn(async move {
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    () = task_cancel.cancelled() => break,
                    Some(_) = streams.join_next(), if !streams.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((mut stream, origin)) = accepted else { break; };
                        if streams.len() >= 128 { continue; }
                        let node = node.clone(); let target = target.clone(); let cancel = task_cancel.clone();
                        streams.spawn(async move {
                            tokio::select! {
                                () = cancel.cancelled() => {},
                                result = async {
                                    // Limit establishment, never the lifetime of an active forwarded stream.
                                    let channel = tokio::time::timeout(Duration::from_secs(30), async {
                                        let dynamic = target.is_none();
                                        let (host, port) = match target { Some(target) => target, None => socks_target(&mut stream).await? };
                                        let channel = node.handle().await.channel_open_direct_tcpip(host, u32::from(port), origin.ip().to_string(), u32::from(origin.port())).await;
                                        let channel = match channel {
                                            Ok(channel) => channel,
                                            Err(error) => { if dynamic { stream.write_all(&[5,5,0,1,0,0,0,0,0,0]).await?; } return Err(error.into()); }
                                        };
                                        if dynamic { stream.write_all(&[5,0,0,1,0,0,0,0,0,0]).await?; }
                                        Ok::<_, anyhow::Error>(channel)
                                    }).await??;
                                    tokio::io::copy_bidirectional(&mut stream, &mut channel.into_stream()).await?;
                                    Ok::<_, anyhow::Error>(())
                                } => { if let Err(error) = result { tracing::debug!(%error, "forwarded stream ended"); } },
                            }
                        });
                    },
                }
            }
            streams.shutdown().await;
        });
        Ok(Forward {
            address,
            cancel,
            _task: task,
        })
    }
}

#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub name: String,
    pub directory: bool,
    pub size: u64,
}

pub struct Forward {
    pub address: SocketAddr,
    pub cancel: CancellationToken,
    _task: tokio::task::JoinHandle<()>,
}
impl Drop for Forward {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn copy(
    source: &mut (impl tokio::io::AsyncRead + Unpin),
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    cancel: &CancellationToken,
    progress: &Sender<u64>,
    limit: Option<std::num::NonZeroU64>,
) -> Result<()> {
    // This buffer lives across awaits. Keeping it inside the future multiplies
    // stack moves through upload/download/select on Windows' smaller thread stacks.
    let mut buffer = vec![0; 64 * 1024];
    let mut copied = 0;
    let started = tokio::time::Instant::now();
    // Small chunks keep cancellation and progress responsive at low limits.
    let chunk = limit.map_or(buffer.len(), |rate| {
        (rate.get() / 10).clamp(1, buffer.len() as u64) as usize
    });
    loop {
        let count = tokio::select! { biased; () = cancel.cancelled() => bail!("传输已取消"), count = source.read(&mut buffer[..chunk]) => count? };
        if count == 0 {
            break;
        }
        if let Some(rate) = limit {
            let deadline = started
                + Duration::from_secs_f64((copied + count as u64) as f64 / rate.get() as f64);
            tokio::select! { biased; () = cancel.cancelled() => bail!("传输已取消"), () = tokio::time::sleep_until(deadline) => {} }
        }
        tokio::select! { biased; () = cancel.cancelled() => bail!("传输已取消"), result = output.write_all(&buffer[..count]) => result? }
        copied += count as u64;
        let _ = progress.try_send(copied);
    }
    tokio::select! { biased; () = cancel.cancelled() => bail!("传输已取消"), result = output.flush() => result? }
    Ok(())
}

async fn socks_target(stream: &mut TcpStream) -> Result<(String, u16)> {
    ensure!(stream.read_u8().await? == 5, "unsupported SOCKS version");
    let count = stream.read_u8().await?;
    let mut methods = vec![0; usize::from(count)];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 255]).await?;
        bail!("SOCKS client requires authentication");
    }
    stream.write_all(&[5, 0]).await?;
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    ensure!(header[..3] == [5, 1, 0], "only SOCKS CONNECT is supported");
    let host = match header[3] {
        1 => {
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await?;
            IpAddr::from(bytes).to_string()
        }
        3 => {
            let length = stream.read_u8().await?;
            ensure!(length > 0, "empty SOCKS hostname");
            let mut bytes = vec![0; usize::from(length)];
            stream.read_exact(&mut bytes).await?;
            String::from_utf8(bytes)?
        }
        4 => {
            let mut bytes = [0; 16];
            stream.read_exact(&mut bytes).await?;
            IpAddr::from(bytes).to_string()
        }
        _ => bail!("unsupported SOCKS address type"),
    };
    Ok((host, stream.read_u16().await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_hosts_only_records_plain_host_names() {
        for host in [
            "server.example",
            "10.0.0.5",
            "::1",
            "fe80::1%en0",
            "中文.example",
        ] {
            assert!(recordable_host(host), "{host}");
        }
        // Separators would trust several hosts at once; patterns never match exactly.
        for host in [
            "",
            "a,b",
            "a b",
            "a\tb",
            "*.example",
            "h?st",
            "!neg",
            "|1|hash",
            "[x]",
            "a#b",
            "a\nb",
        ] {
            assert!(!recordable_host(host), "{host:?}");
        }
    }
    #[test]
    fn transfer_future_keeps_the_io_buffer_off_the_thread_stack() {
        let mut source = tokio::io::empty();
        let mut destination = tokio::io::sink();
        let cancel = CancellationToken::new();
        let (progress, _) = async_channel::bounded(1);
        let transfer = copy(&mut source, &mut destination, &cancel, &progress, None);
        assert!(
            std::mem::size_of_val(&transfer) < 8 * 1024,
            "bounded transfer buffer must be heap-allocated"
        );
    }
    #[test]
    fn jump_routes_are_ordered_and_cycles_fail_before_connecting() -> Result<()> {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let connection = |id, jump| Connection {
            kind: crate::config::ConnectionKind::Ssh,
            initial_directory: ".".into(),
            id,
            name: "node".into(),
            group: String::new(),
            host: "example.test".into(),
            port: 22,
            user: "alice".into(),
            auth: Authentication::Agent,
            jump,
            ssh: Default::default(),
            proxy: None,
        };
        let mut connections = vec![connection(b, Some(a)), connection(a, None)];
        assert_eq!(
            route(b, &connections)?
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            vec![a, b]
        );
        connections[1].jump = Some(b);
        assert_eq!(
            route(b, &connections).err().map(|error| error.to_string()),
            Some("跳板连接存在循环".into())
        );
        Ok(())
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn bandwidth_limit_preserves_bytes_and_cancels_during_throttle() -> Result<()> {
        let bytes = vec![42_u8; 4096];
        let mut source = bytes.as_slice();
        let mut output = Vec::new();
        let cancel = CancellationToken::new();
        let (progress, _) = async_channel::bounded(1);
        let started = tokio::time::Instant::now();
        copy(
            &mut source,
            &mut output,
            &cancel,
            &progress,
            std::num::NonZeroU64::new(4096),
        )
        .await?;
        assert_eq!(output, bytes);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "limit must pace writes, including the first chunk"
        );
        let mut source = bytes.as_slice();
        let mut output = Vec::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            stop.cancel();
        });
        let started = tokio::time::Instant::now();
        assert!(
            copy(
                &mut source,
                &mut output,
                &cancel,
                &progress,
                std::num::NonZeroU64::new(1)
            )
            .await
            .is_err()
        );
        assert!(output.is_empty());
        assert_eq!(started.elapsed(), Duration::from_millis(25));
        Ok(())
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod integration_tests;
