//! Real loopback SSH protocol coverage with ephemeral keys and owned test servers.

use super::*;
use russh::client::Handler as _;
use russh::server::{self, Server as _, Session, VerifiedPublicKey};

#[path = "sftp_test_server.rs"]
mod sftp_fixture;
use sftp_fixture::Files;
#[path = "native_shell_fixture.rs"]
mod native_shell_fixture;
use native_shell_fixture::NativeShell;

struct TestServer {
    address: SocketAddr,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    key: keys::PublicKey,
    files: Files,
    closed_channels: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Copy)]
enum TerminalFixture {
    Echo,
    RejectPty,
    RejectShell,
    SilentShell,
    Gateway,
    Tracked,
    DirectoryGateway,
    Native { shell: &'static str, gateway: bool },
}

impl TestServer {
    async fn start() -> Result<Self> {
        Self::start_with_terminal(TerminalFixture::Echo).await
    }
    async fn start_with_terminal(terminal: TerminalFixture) -> Result<Self> {
        Self::start_authorized(terminal, None).await
    }
    async fn start_authorized(
        terminal: TerminalFixture,
        authorized: Option<keys::PublicKey>,
    ) -> Result<Self> {
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let public_key = key.public_key().clone();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let files = Files::seeded();
        let closed_channels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut server = EchoServer(
            cancel.clone(),
            files.clone(),
            terminal,
            authorized,
            closed_channels.clone(),
        );
        let stopping = cancel.clone();
        let task = tokio::spawn(async move {
            let running = server.run_on_socket(config, &listener);
            let handle = running.handle();
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => { result.expect("loopback SSH server must remain available"); },
                () = stopping.cancelled() => { handle.shutdown("test complete".into()); let _ = running.await; },
            }
        });
        Ok(Self {
            address,
            cancel,
            task,
            key: public_key,
            files,
            closed_channels,
        })
    }
}
impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

struct EchoServer(
    CancellationToken,
    Files,
    TerminalFixture,
    Option<keys::PublicKey>,
    Arc<std::sync::atomic::AtomicUsize>,
);
impl server::Server for EchoServer {
    type Handler = EchoHandler;
    fn new_client(&mut self, _: Option<SocketAddr>) -> Self::Handler {
        EchoHandler {
            cancel: self.0.clone(),
            terminal_channels: HashSet::new(),
            remote_routes: HashMap::new(),
            session_channels: HashMap::new(),
            files: self.1.clone(),
            auth_round: 0,
            terminal_fixture: self.2,
            nested_shell: false,
            other_shell: false,
            authorized: self.3.clone(),
            native_shells: HashMap::new(),
            terminal_sizes: HashMap::new(),
            closed_channels: self.4.clone(),
        }
    }
}
struct EchoHandler {
    cancel: CancellationToken,
    terminal_channels: HashSet<russh::ChannelId>,
    remote_routes: HashMap<u32, CancellationToken>,
    session_channels: HashMap<russh::ChannelId, russh::Channel<server::Msg>>,
    files: Files,
    auth_round: u8,
    terminal_fixture: TerminalFixture,
    nested_shell: bool,
    other_shell: bool,
    authorized: Option<keys::PublicKey>,
    native_shells: HashMap<russh::ChannelId, NativeShell>,
    terminal_sizes: HashMap<russh::ChannelId, crate::terminal::TermSize>,
    closed_channels: Arc<std::sync::atomic::AtomicUsize>,
}
impl server::Handler for EchoHandler {
    type Error = anyhow::Error;
    async fn channel_close(
        &mut self,
        channel: russh::ChannelId,
        session: &mut Session,
    ) -> Result<()> {
        self.session_channels.remove(&channel);
        if self.terminal_channels.remove(&channel) {
            self.closed_channels
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.native_shells.remove(&channel);
        self.terminal_sizes.remove(&channel);
        session.close(channel)?;
        Ok(())
    }
    async fn auth_password(&mut self, _: &str, password: &str) -> Result<server::Auth> {
        Ok(if password == "probe-password" {
            server::Auth::Accept
        } else {
            server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }
        })
    }
    async fn auth_publickey(
        &mut self,
        _: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<server::Auth> {
        Ok(
            if self
                .authorized
                .as_ref()
                .is_none_or(|allowed| allowed == key.public_key)
            {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<server::Auth> {
        if let Some(mut response) = response {
            let answer = response
                .next()
                .context("fixture needs a challenge response")?;
            let expected: &[u8] = if self.auth_round == 1 {
                b"fixture-password"
            } else {
                b"123456"
            };
            if answer.as_ref() != expected {
                return Ok(server::Auth::reject());
            }
            if self.auth_round == 2 {
                return Ok(server::Auth::Accept);
            }
        }
        self.auth_round += 1;
        Ok(server::Auth::Partial {
            name: "Loopback challenge".into(),
            instructions: "Enter fixture credentials".into(),
            prompts: vec![(
                if self.auth_round == 1 {
                    "Password:".into()
                } else {
                    "Verification code:".into()
                },
                false,
            )]
            .into(),
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<()> {
        self.session_channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _: &str,
        columns: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        if matches!(self.terminal_fixture, TerminalFixture::RejectPty) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        self.terminal_sizes.insert(
            channel,
            crate::terminal::TermSize {
                columns: columns.try_into()?,
                rows: rows.try_into()?,
            },
        );
        session.channel_success(channel)?;
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        channel: russh::ChannelId,
        columns: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<()> {
        self.terminal_sizes.insert(
            channel,
            crate::terminal::TermSize {
                columns: columns.try_into()?,
                rows: rows.try_into()?,
            },
        );
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut Session,
    ) -> Result<()> {
        if matches!(self.terminal_fixture, TerminalFixture::RejectShell) {
            session.channel_failure(channel)?;
            return Ok(());
        }
        if matches!(self.terminal_fixture, TerminalFixture::SilentShell) {
            self.terminal_channels.insert(channel);
            return Ok(());
        }
        self.terminal_channels.insert(channel);
        // Real servers may emit a banner before acknowledging the shell request.
        session.data(channel, b"shell-banner\r\n".to_vec())?;
        session.channel_success(channel)?;
        let prompt: &[u8] = match self.terminal_fixture {
            TerminalFixture::Echo
            | TerminalFixture::RejectPty
            | TerminalFixture::RejectShell
            | TerminalFixture::SilentShell => b"ready\r\n",
            TerminalFixture::Gateway => b"gateway$ \x1b]133;B\x07",
            TerminalFixture::Tracked => b"\x1b]0;admin@target: ~\x07admin@target:~$ ",
            TerminalFixture::DirectoryGateway | TerminalFixture::Native { gateway: true, .. } => {
                b"Select target [1]: "
            }
            TerminalFixture::Native {
                shell,
                gateway: false,
            } => {
                self.native_shells.insert(
                    channel,
                    NativeShell::start(
                        shell,
                        channel,
                        session.handle(),
                        self.terminal_sizes[&channel],
                    )?,
                );
                return Ok(());
            }
        };
        session.data(channel, prompt.to_vec())?;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        if matches!(
            self.terminal_fixture,
            TerminalFixture::Gateway
                | TerminalFixture::Tracked
                | TerminalFixture::DirectoryGateway
                | TerminalFixture::Native { gateway: true, .. }
        ) {
            session.channel_failure(channel)?;
        } else {
            ensure!(
                command == crate::session_environment::SSH_PROBE.as_bytes(),
                "unexpected probe command"
            );
            session.channel_success(channel)?;
            session.data(channel, b"NUTTY_ENV_V1\n/bin/".to_vec())?;
            let shell = if let TerminalFixture::Native { shell, .. } = self.terminal_fixture {
                shell
            } else {
                "sh"
            };
            session.data(
                channel,
                format!("{shell}\nLinux\n6.8.0\nx86_64\n").into_bytes(),
            )?;
        }
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<()> {
        ensure!(name == "sftp", "fixture supports only the SFTP subsystem");
        let stream = self
            .session_channels
            .remove(&channel)
            .context("SFTP channel must have been opened")?
            .into_stream();
        session.channel_success(channel)?;
        let handler = self.files.handler();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            tokio::select! { () = cancel.cancelled() => {}, () = russh_sftp::server::run(stream, handler) => {} }
        });
        Ok(())
    }
    async fn data(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        if let Some(shell) = self.native_shells.get(&channel) {
            return shell.send(data).await;
        }
        let mut payload = std::borrow::Cow::Borrowed(data);
        if let Some(envelope) = data.strip_prefix(b" printf '\\033]777;nutty-bootstrap;") {
            let envelope = std::str::from_utf8(envelope)?;
            let (token, source) = envelope
                .split_once("\\007'; ")
                .context("bootstrap envelope")?;
            Uuid::parse_str(token)?;
            session.data(
                channel,
                format!("\x1b]777;nutty-bootstrap;{token}\x07").into_bytes(),
            )?;
            // Native fixtures above execute the wire command; transport-only fixtures
            // acknowledge the envelope and retain their existing command responses.
            payload = std::borrow::Cow::Owned(format!(" {source}").into_bytes());
        }
        let data = payload.as_ref();
        if self.terminal_channels.contains(&channel) {
            let response = match self.terminal_fixture {
                TerminalFixture::DirectoryGateway if !self.nested_shell && data == b"1\r" => {
                    self.nested_shell = true;
                    b"\r\n\x1b]0;admin@target: ~\x07admin@target:~$ ".to_vec()
                }
                TerminalFixture::Tracked | TerminalFixture::DirectoryGateway
                    if data.starts_with(b" printf '\\033]777;nutty-cwd;") =>
                {
                    ensure!(
                        !matches!(self.terminal_fixture, TerminalFixture::DirectoryGateway)
                            || self.nested_shell,
                        "directory queries must never reach a gateway menu"
                    );
                    let command = std::str::from_utf8(data)?;
                    let token = command
                        .split("nutty-cwd;")
                        .nth(1)
                        .and_then(|part| part.split(';').next())
                        .context("tracking token")?;
                    Uuid::parse_str(token)?;
                    let (home, target) = if self.other_shell {
                        ("/srv/ops", "ops@other")
                    } else {
                        ("/home/admin", "admin@target")
                    };
                    format!("\r\n\x1b]777;nutty-cwd;{token};{home}\x07\x1b]777;nutty-home;{token};{home}\x07{target}:~$ ").into_bytes()
                }
                TerminalFixture::Tracked | TerminalFixture::DirectoryGateway
                    if data == b"cd logs\r" =>
                {
                    b"\r\n\x1b]0;admin@target: ~/logs\x07admin@target:~/logs$ ".to_vec()
                }
                TerminalFixture::Tracked | TerminalFixture::DirectoryGateway
                    if data == b"ssh other\r" =>
                {
                    self.other_shell = true;
                    b"\r\n\x1b]0;ops@other: ~\x07ops@other:~$ ".to_vec()
                }
                TerminalFixture::Tracked | TerminalFixture::DirectoryGateway => {
                    b"\r\nunexpected-input\r\n".to_vec()
                }
                // This fixture tests transport bytes, not a real shell. Accept its startup hooks.
                TerminalFixture::Echo if data.starts_with(b" if test -n ") => Vec::new(),
                TerminalFixture::Echo
                | TerminalFixture::RejectPty
                | TerminalFixture::RejectShell
                | TerminalFixture::SilentShell => data.to_vec(),
                TerminalFixture::Native {
                    shell,
                    gateway: true,
                } if data == b"1\r" => {
                    session.data(channel, b"1\r\n".to_vec())?;
                    self.native_shells.insert(
                        channel,
                        NativeShell::start(
                            shell,
                            channel,
                            session.handle(),
                            self.terminal_sizes[&channel],
                        )?,
                    );
                    return Ok(());
                }
                TerminalFixture::Native { .. } => {
                    b"Invalid selection\r\nSelect target [1]: ".to_vec()
                }
                TerminalFixture::Gateway if !self.nested_shell && data == b"ssh target\r" => {
                    self.nested_shell = true;
                    b"\r\nadmin@target:~$ ".to_vec()
                }
                TerminalFixture::Gateway if self.nested_shell && data == b"pwd\r" => {
                    b"pwd\r\n/srv/target\r\nadmin@target:~$ ".to_vec()
                }
                TerminalFixture::Gateway
                    if self.nested_shell && data.starts_with(b" printf '\\033]777;nutty-cwd;") =>
                {
                    b"\r\nunintegrated-ready\r\nadmin@target:~$ ".to_vec()
                }
                TerminalFixture::Gateway => b"\r\nunexpected-input\r\n".to_vec(),
            };
            session.data(channel, response)?;
        }
        Ok(())
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: russh::Channel<server::Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<()> {
        ensure!(
            host == "127.0.0.1",
            "test server only forwards loopback traffic"
        );
        let mut socket = TcpStream::connect((host, u16::try_from(port)?)).await?;
        reply.accept().await;
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            tokio::select! {()=cancel.cancelled()=>{},_=tokio::io::copy_bidirectional(&mut socket,&mut stream)=>{}}
        });
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool> {
        let listener = TcpListener::bind((address, u16::try_from(*port)?)).await?;
        let address = listener.local_addr()?;
        *port = u32::from(address.port());
        let cancel = self.cancel.child_token();
        self.remote_routes.insert(*port, cancel.clone());
        let handle = session.handle();
        tokio::spawn(async move {
            loop {
                let accepted =
                    tokio::select! {()=cancel.cancelled()=>break,result=listener.accept()=>result};
                let Ok((mut socket, origin)) = accepted else {
                    break;
                };
                let channel = handle
                    .channel_open_forwarded_tcpip(
                        address.ip().to_string(),
                        u32::from(address.port()),
                        origin.ip().to_string(),
                        u32::from(origin.port()),
                    )
                    .await;
                if let Ok(channel) = channel {
                    let stopping = cancel.clone();
                    tokio::spawn(async move {
                        let mut stream = channel.into_stream();
                        tokio::select! {()=stopping.cancelled()=>{},_=tokio::io::copy_bidirectional(&mut socket,&mut stream)=>{}}
                    });
                }
            }
        });
        if matches!(self.terminal_fixture, TerminalFixture::SilentShell) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(true)
    }

    async fn cancel_tcpip_forward(&mut self, _: &str, port: u32, _: &mut Session) -> Result<bool> {
        if let Some(cancel) = self.remote_routes.remove(&port) {
            cancel.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn connection(server: &TestServer, key: PathBuf, id: u128, jump: Option<Uuid>) -> Connection {
    Connection {
        kind: crate::config::ConnectionKind::Ssh,
        initial_directory: ".".into(),
        id: Uuid::from_u128(id),
        name: format!("test-{id}"),
        group: String::new(),
        host: "127.0.0.1".into(),
        port: server.address.port(),
        user: "fixture".into(),
        auth: Authentication::Key {
            path: key,
            certificate: None,
        },
        jump,
        ssh: Default::default(),
        proxy: None,
    }
}

async fn wait_for_text(terminal: &Terminal, expected: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let text: String = terminal
                .engine
                .snapshot()
                .cells
                .iter()
                .map(|cell| cell.cell.c)
                .collect();
            if text.contains(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("SSH terminal did not deliver expected text")
}

#[tokio::test]
async fn host_identity_requires_first_use_approval_and_rejects_changed_keys() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("known_hosts");
    let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?
        .public_key()
        .clone();
    let changed = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?
        .public_key()
        .clone();
    assert!(!verify_host_key("host.test", 2222, &key, &path)?);
    let (prompts, receiver) = async_channel::bounded(1);
    let mut client = Client {
        host: "host.test".into(),
        port: 2222,
        known_hosts: path.clone(),
        prompts,
        remote_routes: Arc::new(Mutex::new(HashMap::new())),
        remote_budget: Arc::new(tokio::sync::Semaphore::new(128)),
    };
    let expected = key.fingerprint(keys::HashAlg::Sha256).to_string();
    let accepted = async {
        let prompt = receiver
            .recv()
            .await
            .expect("first use must ask for identity approval");
        assert_eq!(prompt.host, "host.test:2222");
        assert_eq!(prompt.fingerprint, expected);
        prompt
            .answer
            .send(true)
            .expect("approval receiver must be pending");
    };
    let public_key = key.clone().into();
    let (result, ()) = tokio::join!(client.check_server_key(&public_key), accepted);
    assert!(result?);
    assert!(verify_host_key("host.test", 2222, &key, &path)?);
    assert_eq!(
        verify_host_key("host.test", 2222, &changed, &path)
            .err()
            .map(|error| error.to_string()),
        Some("服务器密钥与已保存的身份不一致".into())
    );
    assert!(
        receiver.try_recv().is_err(),
        "changed identity must not become a first-use prompt"
    );
    Ok(())
}

#[tokio::test]
async fn composer_commands_cross_a_gateway_after_switching_to_an_unintegrated_target_shell()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start_with_terminal(TerminalFixture::Gateway).await?;
        let identity = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key = directory.path().join("identity");
        std::fs::write(
            &key,
            identity
                .to_openssh(keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        let connection = connection(&server, key, 909, None);
        let (prompts, receive) = async_channel::bounded::<HostPrompt>(1);
        let approve = tokio::spawn(async move {
            let prompt = receive.recv().await?;
            let _ = prompt.answer.send(true);
            Ok::<_, anyhow::Error>(())
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry
            .connect(connection.id, std::slice::from_ref(&connection))
            .await?;
        approve.await??;
        let terminal = node
            .terminal(&Preferences {
                track_directory: false,
                ..Default::default()
            })
            .await?;
        wait_for_text(&terminal, "gateway$").await?;
        let environment = terminal.engine.environment();
        assert_eq!(environment.target, crate::session_environment::Target::Ssh);
        assert_eq!(
            environment.operating_system,
            crate::session_environment::Platform::Unknown,
            "a gateway denying exec must not inherit the Mac client platform"
        );
        terminal.engine.input(b"ssh target\r".to_vec())?;
        wait_for_text(&terminal, "admin@target").await?;
        wait_for_text(&terminal, "unintegrated-ready").await?;
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(!terminal.engine.automatic_command_ready(revision));
        terminal.engine.send_command("pwd")?;
        wait_for_text(&terminal, "/srv/target").await?;
        assert_eq!(
            terminal
                .engine
                .command_history()
                .last()
                .map(|entry| entry.command.as_str()),
            Some("pwd")
        );
        assert!(!terminal.engine.recent_output().contains("unexpected-input"));
        drop(terminal);
        registry.disconnect(connection.id).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("gateway command submission timed out")?
}

#[tokio::test]
async fn shared_channels_jump_routes_and_forwarding_preserve_connection_ownership() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir()?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key_path = directory.path().join("identity");
        std::fs::write(
            &key_path,
            key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
        )?;
        let first = TestServer::start().await?;
        let second = TestServer::start().await?;
        let root = connection(&first, key_path.clone(), 1, None);
        let child = connection(&second, key_path, 2, Some(root.id));
        let (prompts, receiver) = async_channel::bounded(2);
        let fingerprints = HashMap::from([
            (
                first.address.port(),
                first.key.fingerprint(keys::HashAlg::Sha256).to_string(),
            ),
            (
                second.address.port(),
                second.key.fingerprint(keys::HashAlg::Sha256).to_string(),
            ),
        ]);
        let approving = tokio::spawn(async move {
            for _ in 0..2 {
                let prompt: HostPrompt = receiver
                    .recv()
                    .await
                    .expect("each SSH hop requires approval");
                let port: u16 = prompt
                    .host
                    .rsplit_once(':')
                    .expect("host includes port")
                    .1
                    .parse()
                    .expect("numeric port");
                assert_eq!(Some(&prompt.fingerprint), fingerprints.get(&port));
                let _ = prompt.answer.send(true);
            }
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry
            .connect(child.id, &[root.clone(), child.clone()])
            .await?;
        approving.await?;
        let terminal = node.terminal(&Preferences::default()).await?;
        let surviving = node.terminal(&Preferences::default()).await?;
        let environment = terminal.engine.environment();
        assert_eq!(environment.target, crate::session_environment::Target::Ssh);
        assert_eq!(
            environment.operating_system,
            crate::session_environment::Platform::Linux
        );
        assert_eq!(environment.kernel_release.as_deref(), Some("6.8.0"));
        assert_eq!(environment.startup_shell.as_deref(), Some("/bin/sh"));
        assert_eq!(environment.architecture.as_deref(), Some("x86_64"));
        assert!(
            terminal.engine.command_history().is_empty(),
            "metadata probes never enter the terminal command history"
        );
        terminal.engine.input(b"first-channel".to_vec())?;
        wait_for_text(&terminal, "first-channel").await?;
        drop(terminal);
        surviving.engine.input(b"surviving-channel".to_vec())?;
        wait_for_text(&surviving, "surviving-channel").await?;
        let echo = TcpListener::bind("127.0.0.1:0").await?;
        let address = echo.local_addr()?;
        let echo_task = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = echo.accept().await?;
                let mut payload = [0; 4];
                socket.read_exact(&mut payload).await?;
                socket.write_all(&payload).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let forward = node
            .forward(
                "127.0.0.1:0".parse()?,
                Some(("127.0.0.1".into(), address.port())),
            )
            .await?;
        let mut socket = TcpStream::connect(forward.address).await?;
        socket.write_all(b"PING").await?;
        let mut response = [0; 4];
        socket.read_exact(&mut response).await?;
        assert_eq!(&response, b"PING");
        drop(socket);
        drop(forward);
        let socks = node.forward("127.0.0.1:0".parse()?, None).await?;
        let mut socket = TcpStream::connect(socks.address).await?;
        socket.write_all(&[5, 1, 0]).await?;
        let mut auth = [0; 2];
        socket.read_exact(&mut auth).await?;
        assert_eq!(auth, [5, 0]);
        let port = address.port().to_be_bytes();
        socket
            .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, port[0], port[1]])
            .await?;
        let mut connected = [0; 10];
        socket.read_exact(&mut connected).await?;
        assert_eq!(connected, [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        socket.write_all(b"PONG").await?;
        socket.read_exact(&mut response).await?;
        assert_eq!(&response, b"PONG");
        drop(socket);
        drop(socks);
        let reserved = TcpListener::bind("127.0.0.1:0").await?;
        let remote_address = reserved.local_addr()?;
        drop(reserved);
        let remote = node
            .forward_remote(remote_address, ("127.0.0.1".into(), address.port()))
            .await?;
        let mut socket = TcpStream::connect(remote.address).await?;
        socket.write_all(b"BACK").await?;
        socket.read_exact(&mut response).await?;
        assert_eq!(&response, b"BACK");
        drop(socket);
        drop(remote);
        echo_task.await??;
        registry.disconnect(root.id).await?;
        assert!(
            node.cancel.is_cancelled(),
            "disconnecting a jump parent must cancel its children"
        );
        assert!(
            registry.nodes.lock().await.is_empty(),
            "disconnected routes must release registry ownership"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("loopback SSH workflow timed out")?
}

#[tokio::test]
async fn canceled_remote_forward_start_releases_route_and_listener() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let directory = tempfile::tempdir()?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key_path = directory.path().join("identity");
        std::fs::write(
            &key_path,
            key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
        )?;
        let server = TestServer::start_with_terminal(TerminalFixture::SilentShell).await?;
        let connection = connection(&server, key_path, 41, None);
        let (prompts, receiver) = async_channel::bounded::<HostPrompt>(1);
        let approving = tokio::spawn(async move {
            let prompt = receiver.recv().await.expect("host prompt");
            let _ = prompt.answer.send(true);
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry.connect(connection.id, &[connection]).await?;
        approving.await?;
        let reserved = TcpListener::bind("127.0.0.1:0").await?;
        let address = reserved.local_addr()?;
        drop(reserved);
        let timeout = tokio::time::timeout(
            Duration::from_millis(80),
            node.forward_remote(address, ("127.0.0.1".into(), 80)),
        )
        .await;
        assert!(
            timeout.is_err(),
            "cancel while the server delays its success reply"
        );
        while !node.remote_routes.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The same port must be usable after the cancellation acknowledgement.
        let forward = node
            .forward_remote(address, ("127.0.0.1".into(), 80))
            .await?;
        drop(forward);
        while !node.remote_routes.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let listener = TcpListener::bind(address).await?;
        drop(listener);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("remote forwarding cancellation timed out")?
}

#[tokio::test]
async fn sftp_transfers_publish_exact_bytes_without_clobbering_and_cancel_cleanly() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let directory = tempfile::tempdir()?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key_path = directory.path().join("identity");
        std::fs::write(
            &key_path,
            key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
        )?;
        let server = TestServer::start().await?;
        let mut connection = connection(&server, key_path, 3, None);
        connection.kind = config::ConnectionKind::Sftp;
        let (prompts, receiver) = async_channel::bounded(1);
        let approving = tokio::spawn(async move {
            let prompt: HostPrompt = receiver
                .recv()
                .await
                .expect("server identity needs approval");
            let _ = prompt.answer.send(true);
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry.connect(connection.id, &[connection]).await?;
        approving.await?;
        let mut invalid = node.connection.clone();
        invalid.initial_directory = "missing-directory".into();
        assert!(
            registry
                .probe(
                    invalid.id,
                    &[invalid],
                    &[],
                    zeroize::Zeroizing::new(String::new()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(
            !node.cancel.is_cancelled(),
            "isolated SFTP probe must preserve the active connection"
        );
        let files = node.list(".").await?;
        assert_eq!(
            files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["folder", "large.bin", "source.txt"]
        );
        node.file_operation(FileOperation::Directory("new-folder".into()))
            .await?;
        node.file_operation(FileOperation::Remove {
            path: "new-folder".into(),
            directory: true,
        })
        .await?;
        node.file_operation(FileOperation::Rename {
            source: "source.txt".into(),
            target: "renamed.txt".into(),
        })
        .await?;
        node.file_operation(FileOperation::Permissions {
            path: "renamed.txt".into(),
            mode: 0o640,
        })
        .await?;
        assert_eq!(
            node.sftp()
                .await?
                .metadata("renamed.txt")
                .await?
                .permissions
                .map(|m| m & 0o777),
            Some(0o640)
        );
        assert!(
            node.file_operation(FileOperation::Rename {
                source: "renamed.txt".into(),
                target: "large.bin".into()
            })
            .await
            .is_err()
        );
        node.file_operation(FileOperation::Rename {
            source: "renamed.txt".into(),
            target: "source.txt".into(),
        })
        .await?;
        assert_eq!(node.read_text("source.txt").await?, "remote-source\n");
        let cancel = CancellationToken::new();
        let (progress, _) = async_channel::bounded(16);
        let downloaded = directory.path().join("downloaded.txt");
        node.download("source.txt", &downloaded, &cancel, &progress, None)
            .await?;
        assert_eq!(tokio::fs::read(&downloaded).await?, b"remote-source\n");
        assert!(
            node.download("large.bin", &downloaded, &cancel, &progress, None)
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(&downloaded).await?,
            b"remote-source\n",
            "an existing download must remain intact"
        );
        let local = directory.path().join("upload.txt");
        tokio::fs::write(&local, b"local-upload\n").await?;
        node.upload(&local, "uploaded.txt", &cancel, &progress, None)
            .await?;
        assert!(
            node.upload(&local, "source.txt", &cancel, &progress, None)
                .await
                .is_err()
        );
        let snapshot = server.files.snapshot().await;
        assert_eq!(snapshot["uploaded.txt"], b"local-upload\n");
        assert_eq!(snapshot["source.txt"], b"remote-source\n");
        node.save_text("source.txt", "remote-source\n", "edited-source\n")
            .await?;
        assert!(
            node.save_text("source.txt", "remote-source\n", "stale-edit\n")
                .await
                .is_err(),
            "stale edits must detect changed remote content"
        );
        assert_eq!(node.read_text("source.txt").await?, "edited-source\n");

        let stopped = CancellationToken::new();
        stopped.cancel();
        let absent = directory.path().join("never-created.txt");
        assert!(
            node.download("source.txt", &absent, &stopped, &progress, None)
                .await
                .is_err()
        );
        assert!(
            node.upload(&local, "never-created.txt", &stopped, &progress, None)
                .await
                .is_err()
        );
        assert!(!absent.exists());

        let (progress, received) = async_channel::bounded(16);
        let stop = CancellationToken::new();
        let interrupted = directory.path().join("interrupted.bin");
        let downloading = node.download("large.bin", &interrupted, &stop, &progress, None);
        let cancelling = async {
            received
                .recv()
                .await
                .expect("transfer must report actual progress");
            stop.cancel();
        };
        let (result, ()) = tokio::join!(downloading, cancelling);
        assert!(
            result.is_err(),
            "cancellation must stop an in-flight download"
        );
        assert!(
            !interrupted.exists(),
            "cancelled transfer must not publish a partial target"
        );
        let large_local = directory.path().join("large-upload.bin");
        tokio::fs::write(&large_local, vec![23; 4 * 1024 * 1024]).await?;
        let (progress, received) = async_channel::bounded(16);
        let stop = CancellationToken::new();
        let uploading = node.upload(
            &large_local,
            "interrupted-upload.bin",
            &stop,
            &progress,
            None,
        );
        let cancelling = async {
            received
                .recv()
                .await
                .expect("upload must begin before cancellation");
            stop.cancel();
        };
        let (result, ()) = tokio::join!(uploading, cancelling);
        assert!(
            result.is_err(),
            "cancellation must stop an in-flight upload"
        );
        assert!(
            !server
                .files
                .snapshot()
                .await
                .contains_key("interrupted-upload.bin"),
            "cancelled upload must not publish a target"
        );
        assert!(
            !std::fs::read_dir(directory.path())?.any(|entry| entry
                .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".part"))),
            "temporary downloads must be removed"
        );
        assert!(
            !server
                .files
                .snapshot()
                .await
                .keys()
                .any(|name| name.ends_with(".part")),
            "remote temporary uploads must be removed"
        );
        registry.disconnect(node.connection.id).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("SFTP workflow timed out")?
}

#[tokio::test]
async fn interactive_authentication_delivers_multiple_masked_challenges_and_can_cancel()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start().await?;
        let mut connection = connection(&server, PathBuf::new(), 4, None);
        connection.auth = Authentication::Interactive;
        let (prompts, receiver) = async_channel::bounded(1);
        let (auth, challenges) = async_channel::bounded(1);
        let registry = Arc::new(Registry::new(directory.path(), prompts).with_authentication(auth));
        let answering = tokio::spawn(async move {
            receiver
                .recv()
                .await
                .expect("identity prompt")
                .answer
                .send(true)
                .expect("identity pending");
            for (label, answer) in [
                ("Password:", "fixture-password"),
                ("Verification code:", "123456"),
            ] {
                let challenge: AuthPrompt =
                    challenges.recv().await.expect("authentication challenge");
                assert_eq!(challenge.prompts[0].prompt, label);
                assert!(!challenge.prompts[0].echo, "secrets must remain masked");
                challenge
                    .answer
                    .send(Some(vec![answer.into()]))
                    .expect("challenge pending");
            }
            let challenge = challenges
                .recv()
                .await
                .expect("second connection needs authentication");
            challenge.answer.send(None).expect("cancellation pending");
        });
        let node = registry
            .connect(connection.id, std::slice::from_ref(&connection))
            .await?;
        let terminal = node.terminal(&Preferences::default()).await?;
        wait_for_text(&terminal, "ready").await?;
        registry.disconnect(connection.id).await?;
        assert!(
            registry
                .connect(connection.id, &[connection])
                .await
                .is_err(),
            "cancelling authentication must reject the connection"
        );
        assert!(
            registry.nodes.lock().await.is_empty(),
            "cancelled connection must not enter the registry"
        );
        answering.await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("interactive authentication timed out")?
}

#[tokio::test]
async fn transport_loss_reconnects_shared_nodes_without_losing_terminal_history() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start().await?;
        let identity = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key = directory.path().join("identity");
        std::fs::write(
            &key,
            identity
                .to_openssh(keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        let connection = connection(&server, key, 90, None);
        let (prompts, receive) = async_channel::bounded::<HostPrompt>(1);
        let approve = tokio::spawn(async move {
            let prompt = receive.recv().await?;
            let _ = prompt.answer.send(true);
            Ok::<_, anyhow::Error>(())
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry
            .connect(connection.id, std::slice::from_ref(&connection))
            .await?;
        approve.await??;
        let first = node.terminal(&Preferences::default()).await?;
        let second = node.terminal(&Preferences::default()).await?;
        first.engine.input(b"preserved-one".to_vec())?;
        second.engine.input(b"preserved-two".to_vec())?;
        wait_for_text(&first, "preserved-one").await?;
        wait_for_text(&second, "preserved-two").await?;
        node.handle()
            .await
            .disconnect(
                russh::Disconnect::ByApplication,
                "fixture network loss",
                "en",
            )
            .await?;
        wait_for_text(&first, "[reconnected]").await?;
        wait_for_text(&second, "[reconnected]").await?;
        assert!(!first.engine.snapshot().closed);
        assert!(!second.engine.snapshot().closed);
        assert!(Arc::ptr_eq(
            &node,
            &registry.connect(connection.id, &[connection]).await?
        ));
        first.engine.input(b"after-recovery".to_vec())?;
        wait_for_text(&first, "after-recovery").await?;
        assert!(first.engine.recent_output().contains("preserved-one"));
        assert!(!first.engine.recent_output().contains("preserved-two"));
        registry.disconnect(node.connection.id).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("automatic SSH recovery timed out")?
}

#[tokio::test]
async fn connection_probe_checks_each_hop_without_touching_live_nodes_or_credentials() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir()?;
        let first = TestServer::start().await?;
        let second = TestServer::start().await?;
        let third = TestServer::start().await?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let path = directory.path().join("key");
        std::fs::write(
            &path,
            key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
        )?;
        let root = connection(&first, path.clone(), 901, None);
        let middle = connection(&second, path.clone(), 902, Some(root.id));
        let mut target = connection(&third, path, 903, Some(middle.id));
        target.auth = Authentication::Password;
        let (prompts, receive) = async_channel::bounded(4);
        let approving = tokio::spawn(async move {
            for _ in 0..3 {
                let prompt: HostPrompt =
                    receive.recv().await.expect("three distinct hop identities");
                let _ = prompt.answer.send(true);
            }
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let live = registry
            .connect(root.id, std::slice::from_ref(&root))
            .await?;
        let route = [root.clone(), middle, target.clone()];
        registry
            .probe(
                target.id,
                &route,
                &[],
                zeroize::Zeroizing::new("probe-password".into()),
                CancellationToken::new(),
            )
            .await?;
        approving.await?;
        assert_eq!(
            registry.nodes.lock().await.len(),
            1,
            "probe nodes never enter the live registry"
        );
        assert!(!live.cancel.is_cancelled());
        assert!(!live.handle().await.is_closed());
        assert!(
            registry
                .probe(
                    target.id,
                    &route,
                    &[],
                    zeroize::Zeroizing::new("wrong-password".into()),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(
            !live.cancel.is_cancelled(),
            "failed probes also leave live sessions intact"
        );
        registry.disconnect(root.id).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn handshake_timeout_and_probe_cancellation_leave_no_shared_nodes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (prompts, _) = async_channel::bounded(1);
    let registry = Arc::new(Registry::new(directory.path(), prompts));
    let connection = Connection {
        kind: crate::config::ConnectionKind::Ssh,
        initial_directory: ".".into(),
        id: Uuid::new_v4(),
        name: "silent SSH server".into(),
        group: String::new(),
        host: "127.0.0.1".into(),
        port: address.port(),
        user: "qa".into(),
        auth: Authentication::Agent,
        jump: None,
        proxy: None,
        ssh: config::SshOptions {
            connect_timeout_seconds: 1,
            ..Default::default()
        },
    };
    let timeout = registry
        .probe(
            connection.id,
            std::slice::from_ref(&connection),
            &[],
            zeroize::Zeroizing::new(String::new()),
            CancellationToken::new(),
        )
        .await;
    assert!(timeout.err().is_some_and(|error| {
        error
            .chain()
            .any(|cause| cause.is::<tokio::time::error::Elapsed>())
    }));
    assert!(registry.nodes.lock().await.is_empty());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(1),
        registry.probe(
            connection.id,
            &[connection],
            &[],
            zeroize::Zeroizing::new(String::new()),
            cancel,
        ),
    )
    .await?;
    assert!(cancelled.is_err());
    assert!(registry.nodes.lock().await.is_empty());
    Ok(())
}

/// Starts only an explicitly requested, isolated native UI fixture.
#[tokio::test]
#[ignore = "native UI fixture, started explicitly with a temporary NUTTY_NATIVE_QA_DIR"]
async fn native_qa_server() -> Result<()> {
    let directory =
        PathBuf::from(std::env::var_os("NUTTY_NATIVE_QA_DIR").context("QA directory required")?);
    ensure!(directory.is_dir(), "QA directory must exist");
    let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
    let key_path = directory.join("identity");
    std::fs::write(
        &key_path,
        key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
    )?;
    let server = TestServer::start().await?;
    #[cfg(unix)]
    let agent = crate::ssh_agent::tests::TestAgent::start().await?;
    #[cfg(unix)]
    agent.add(&key).await?;
    let mut agent_connection = connection(&server, key_path.clone(), 9001, None);
    agent_connection.name = "QA Agent".into();
    #[cfg(unix)]
    {
        agent_connection.auth = Authentication::Agent;
        agent_connection.ssh.agent_socket = Some(agent.socket.clone());
    }
    let mut interactive = connection(&server, key_path.clone(), 9002, None);
    interactive.name = "QA Interactive".into();
    interactive.auth = Authentication::Interactive;
    let mut removable = connection(&server, key_path.clone(), 9003, None);
    removable.name = "QA Removable".into();
    let mut jump = connection(&server, key_path.clone(), 9004, Some(agent_connection.id));
    jump.name = "QA Jump".into();
    let mut store = crate::config::Store::open(directory.clone())?;
    store.profile.preferences.directory = directory.clone();
    store.profile.connections = vec![agent_connection, interactive, removable, jump];
    store.save()?;
    std::fs::write(
        directory.join("server.json"),
        serde_json::to_vec(&serde_json::json!({"port":server.address.port(),"key":key_path}))?,
    )?;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn isolated_agent_signs_selected_keys_and_reports_missing_denied_or_rejected_identities()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(7), async {
        let directory = tempfile::tempdir()?;
        let agent = crate::ssh_agent::tests::TestAgent::start().await?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let other = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let server =
            TestServer::start_authorized(TerminalFixture::Echo, Some(key.public_key().clone()))
                .await?;
        // Trust only this ephemeral server, within this test directory.
        keys::known_hosts::learn_known_hosts_path(
            &server.address.ip().to_string(),
            server.address.port(),
            &server.key,
            directory.path().join("known_hosts"),
        )?;
        let (prompts, _receiver) = async_channel::bounded(1);
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let mut config = connection(&server, PathBuf::new(), 801, None);
        config.auth = Authentication::Agent;
        config.ssh.agent_socket = Some(agent.socket.clone());
        let probe = |config: Connection| {
            let registry = registry.clone();
            async move {
                registry
                    .probe(
                        config.id,
                        &[config],
                        &[],
                        zeroize::Zeroizing::new(String::new()),
                        CancellationToken::new(),
                    )
                    .await
            }
        };
        assert!(
            format!(
                "{:#}",
                probe(config.clone()).await.expect_err("empty Agent")
            )
            .contains("没有密钥")
        );
        agent.add(&key).await?;
        agent.add(&other).await?;
        config.ssh.agent_identity = Some(
            key.public_key()
                .fingerprint(keys::HashAlg::Sha256)
                .to_string(),
        );
        let node = registry
            .connect(config.id, std::slice::from_ref(&config))
            .await?;
        let terminal = node.terminal(&Preferences::default()).await?;
        wait_for_text(&terminal, "ready").await?;
        terminal
            .engine
            .input(b"agent-echo\r".to_vec())
            .map_err(anyhow::Error::msg)?;
        wait_for_text(&terminal, "agent-echo").await?;
        assert!(
            agent.signatures() > 0,
            "actual SSH authentication must use Agent signatures"
        );
        registry.disconnect(config.id).await?;
        let missing = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        config.ssh.agent_identity = Some(
            missing
                .public_key()
                .fingerprint(keys::HashAlg::Sha256)
                .to_string(),
        );
        let signatures = agent.signatures();
        assert!(
            format!(
                "{:#}",
                probe(config.clone()).await.expect_err("missing identity")
            )
            .contains("未加载")
        );
        assert_eq!(
            agent.signatures(),
            signatures,
            "must not silently sign with another identity"
        );
        config.ssh.agent_identity = Some(
            other
                .public_key()
                .fingerprint(keys::HashAlg::Sha256)
                .to_string(),
        );
        assert!(
            format!(
                "{:#}",
                probe(config.clone())
                    .await
                    .expect_err("unauthorized identity")
            )
            .contains("服务器拒绝")
        );
        config.ssh.agent_identity = Some(
            key.public_key()
                .fingerprint(keys::HashAlg::Sha256)
                .to_string(),
        );
        agent.deny_signing();
        assert!(
            format!(
                "{:#}",
                probe(config).await.expect_err("denied Agent signature")
            )
            .contains("签名失败")
        );
        assert!(registry.nodes.lock().await.is_empty());
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Agent SSH fixture timed out")?
}

#[tokio::test]
async fn interactive_probe_rejects_wrong_answers_and_invalidates_cancelled_challenges() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(5), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start().await?;
        keys::known_hosts::learn_known_hosts_path(
            "127.0.0.1",
            server.address.port(),
            &server.key,
            directory.path().join("known_hosts"),
        )?;
        let (prompts, _receiver) = async_channel::bounded(1);
        let (auth, challenges) = async_channel::bounded(1);
        let registry = Arc::new(Registry::new(directory.path(), prompts).with_authentication(auth));
        let mut connection = connection(&server, PathBuf::new(), 802, None);
        connection.auth = Authentication::Interactive;
        let probing = registry.clone();
        let config = connection.clone();
        let task = tokio::spawn(async move {
            probing
                .probe(
                    config.id,
                    &[config],
                    &[],
                    zeroize::Zeroizing::new(String::new()),
                    CancellationToken::new(),
                )
                .await
        });
        let challenge: AuthPrompt = challenges.recv().await?;
        challenge
            .answer
            .send(Some(vec!["incorrect-password".into()]))
            .expect("pending challenge");
        assert!(
            format!("{:#}", task.await?.expect_err("wrong answer must fail"))
                .contains("拒绝交互式认证")
        );
        let cancel = CancellationToken::new();
        let stopping = cancel.clone();
        let probing = registry.clone();
        let task = tokio::spawn(async move {
            probing
                .probe(
                    connection.id,
                    &[connection],
                    &[],
                    zeroize::Zeroizing::new(String::new()),
                    stopping,
                )
                .await
        });
        let challenge = challenges.recv().await?;
        cancel.cancel();
        assert!(format!("{:#}", task.await?.expect_err("cancelled probe")).contains("已取消"));
        assert!(
            challenge.answer.is_closed(),
            "cancelled prompts must disappear from the UI"
        );
        assert!(registry.nodes.lock().await.is_empty());
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("interactive probe fixture timed out")?
}

#[tokio::test]
async fn http_and_socks_proxies_carry_real_ssh_terminal_and_sftp_connections() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(7), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start().await?;
        let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key_path = directory.path().join("key");
        std::fs::write(
            &key_path,
            key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
        )?;
        // Trust the deliberately unresolvable target name, whose DNS belongs to the proxy fixture.
        keys::known_hosts::learn_known_hosts_path(
            "ssh.fixture.invalid",
            server.address.port(),
            &server.key,
            directory.path().join("known_hosts"),
        )?;
        for socks in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let proxy_address = listener.local_addr()?;
            let target = server.address;
            let forwarding = tokio::spawn(async move {
                let (mut downstream, _) = listener.accept().await?;
                if socks {
                    let mut hello = [0; 3];
                    downstream.read_exact(&mut hello).await?;
                    assert_eq!(hello, [5, 1, 0]);
                    downstream.write_all(&[5, 0]).await?;
                    let mut request = [0; 5];
                    downstream.read_exact(&mut request).await?;
                    assert_eq!(&request[..4], &[5, 1, 0, 3]);
                    let mut host = vec![0; usize::from(request[4])];
                    downstream.read_exact(&mut host).await?;
                    assert_eq!(host, b"ssh.fixture.invalid");
                    assert_eq!(downstream.read_u16().await?, target.port());
                    downstream
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                        .await?;
                } else {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(downstream.read_u8().await?);
                    }
                    assert!(std::str::from_utf8(&request)?.starts_with(&format!(
                        "CONNECT ssh.fixture.invalid:{} HTTP/1.1",
                        target.port()
                    )));
                    downstream
                        .write_all(b"HTTP/1.1 200 Connected\r\n\r\n")
                        .await?;
                }
                let mut upstream = TcpStream::connect(target).await?;
                tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await?;
                Ok::<_, anyhow::Error>(())
            });
            let proxy = crate::network::Proxy {
                id: Uuid::new_v4(),
                name: "fixture".into(),
                url: format!(
                    "{}://{proxy_address}",
                    if socks { "socks5h" } else { "http" }
                ),
                username: String::new(),
            };
            let mut config = connection(&server, key_path.clone(), 803, None);
            config.host = "ssh.fixture.invalid".into();
            config.proxy = Some(proxy.id);
            let (prompts, _receiver) = async_channel::bounded(1);
            let registry = Arc::new(Registry::new(directory.path(), prompts));
            let node = registry
                .connect_credentials(
                    config.id,
                    std::slice::from_ref(&config),
                    &[proxy],
                    &HashMap::new(),
                )
                .await?;
            let terminal = node.terminal(&Preferences::default()).await?;
            wait_for_text(&terminal, "ready").await?;
            terminal.engine.input(b"tunnel-echo\r".to_vec())?;
            wait_for_text(&terminal, "tunnel-echo").await?;
            assert!(
                !node.list(".").await?.is_empty(),
                "SFTP must work over the same proxy transport"
            );
            registry.disconnect(config.id).await?;
            forwarding.await??;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("proxied SSH fixture timed out")?
}

async fn wait_for_directory(terminal: &Terminal, expected: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if terminal.engine.snapshot().directory.as_deref() == Some(expected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("directory did not become {expected}"))
}

#[tokio::test]
async fn directory_tracking_follows_direct_jump_and_gateway_target_shells_without_exec()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (jumped, gateway) in [(false, false), (true, false), (false, true)] {
            let directory = tempfile::tempdir()?;
            let parent = TestServer::start().await?;
            let server = TestServer::start_with_terminal(if gateway {
                TerminalFixture::DirectoryGateway
            } else {
                TerminalFixture::Tracked
            })
            .await?;
            let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
            let path = directory.path().join("identity");
            std::fs::write(
                &path,
                key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
            )?;
            for trusted in [&parent, &server] {
                keys::known_hosts::learn_known_hosts_path(
                    "127.0.0.1",
                    trusted.address.port(),
                    &trusted.key,
                    directory.path().join("known_hosts"),
                )?;
            }
            let parent_connection = connection(&parent, path.clone(), 950, None);
            let connection = connection(&server, path, 951, jumped.then_some(parent_connection.id));
            let route = if jumped {
                vec![parent_connection, connection.clone()]
            } else {
                vec![connection.clone()]
            };
            let (prompts, _) = async_channel::bounded(1);
            let registry = Arc::new(Registry::new(directory.path(), prompts));
            let node = registry
                .connect_credentials(connection.id, &route, &[], &HashMap::new())
                .await?;
            let terminal = node.terminal(&Preferences::default()).await?;
            if gateway {
                wait_for_text(&terminal, "Select target").await?;
                assert!(
                    terminal.engine.snapshot().directory.is_none(),
                    "gateway menu is not a target directory"
                );
                terminal.engine.input(b"1\r".to_vec())?;
            }
            wait_for_directory(&terminal, "/home/admin").await?;
            terminal.engine.input(b"cd logs\r".to_vec())?;
            wait_for_directory(&terminal, "/home/admin/logs").await?;
            terminal.engine.input(b"ssh other\r".to_vec())?;
            wait_for_directory(&terminal, "/srv/ops").await?;
            assert!(
                terminal.engine.command_history().is_empty(),
                "metadata queries do not enter user history"
            );
            assert!(!terminal.engine.snapshot().closed);
            registry.disconnect(connection.id).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("directory tracking route fixture timed out")?
}

#[tokio::test]
async fn active_forward_survives_five_minutes_and_stops_on_cancel() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = TestServer::start().await?;
    let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
    let key_path = directory.path().join("identity");
    std::fs::write(
        &key_path,
        key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
    )?;
    keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        server.address.port(),
        &server.key,
        directory.path().join("known_hosts"),
    )?;
    let mut connection = connection(&server, key_path, 8001, None);
    connection.ssh.keepalive_seconds = 0;
    let (prompts, _) = async_channel::bounded(1);
    let registry = Arc::new(Registry::new(directory.path(), prompts));
    let node = registry.connect(connection.id, &[connection]).await?;
    let echo = TcpListener::bind("127.0.0.1:0").await?;
    let address = echo.local_addr()?;
    let echo_task = tokio::spawn(async move {
        let (mut socket, _) = echo.accept().await?;
        let (mut read, mut write) = socket.split();
        tokio::io::copy(&mut read, &mut write).await?;
        Ok::<_, anyhow::Error>(())
    });
    let forward = node
        .forward(
            "127.0.0.1:0".parse()?,
            Some(("127.0.0.1".into(), address.port())),
        )
        .await?;
    let mut socket = TcpStream::connect(forward.address).await?;
    socket.write_all(b"PING").await?;
    let mut bytes = [0; 4];
    socket.read_exact(&mut bytes).await?;
    assert_eq!(&bytes, b"PING");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(290)).await;
    tokio::time::resume();
    socket.write_all(b"LIVE").await?;
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut bytes)).await??;
    assert_eq!(&bytes, b"LIVE");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    socket.write_all(b"NEXT").await?;
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut bytes)).await??;
    assert_eq!(&bytes, b"NEXT");
    forward.cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), socket.read(&mut bytes)).await??,
        0
    );
    assert!(
        !node.handle().await.is_closed(),
        "SSH transport is still healthy"
    );
    drop(forward);
    echo_task.abort();
    Ok(())
}

async fn wait_transfer_snapshot(
    receiver: &mut tokio::sync::watch::Receiver<crate::transfers::Snapshot>,
    predicate: impl Fn(&crate::transfers::Snapshot) -> bool,
) -> Result<crate::transfers::Snapshot> {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = receiver.borrow_and_update().clone();
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            receiver.changed().await?;
        }
    })
    .await
    .context("transfer state deadline")?
}
#[tokio::test]
async fn transfer_queue_schedules_pauses_restores_and_binds_configuration() -> Result<()> {
    use crate::transfers::{Action, Configuration, Direction, Queue, State};
    let directory = tempfile::tempdir()?;
    let server = TestServer::start().await?;
    let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
    let key_path = directory.path().join("identity");
    std::fs::write(
        &key_path,
        key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
    )?;
    keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        server.address.port(),
        &server.key,
        directory.path().join("known_hosts"),
    )?;
    let connection = connection(&server, key_path, 9901, None);
    let (prompts, _) = async_channel::bounded(1);
    let registry = Arc::new(Registry::new(directory.path(), prompts));
    let configuration = Configuration {
        connections: vec![connection.clone()],
        proxies: vec![],
        concurrency: 1,
        limit: 64 * 1024,
    };
    let (queue, mut updates) = Queue::start(
        directory.path().into(),
        registry.clone(),
        configuration.clone(),
    );
    for name in ["one", "two", "three"] {
        queue.enqueue(
            connection.id,
            directory.path().join(name),
            "/large.bin".into(),
            Direction::Download,
        )?;
    }
    let snapshot =
        wait_transfer_snapshot(&mut updates, |s| s.tasks.len() == 3 && s.tasks[0].bytes > 0)
            .await?;
    assert_eq!(
        snapshot.tasks.iter().filter(|t| t.state.active()).count(),
        1
    );
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .filter(|t| t.state == State::Queued)
            .count(),
        2
    );
    let id = snapshot.tasks[0].id;
    queue.action(id, Action::Pause)?;
    let snapshot = wait_transfer_snapshot(&mut updates, |s| {
        s.tasks[0].state == State::Paused && s.tasks[1].bytes > 0
    })
    .await?;
    let second = snapshot.tasks[1].id;
    let third = snapshot.tasks[2].id;
    queue.shutdown().await;
    let mut configuration = configuration;
    configuration.limit = 0;
    let (restored, mut updates) = Queue::start(
        directory.path().into(),
        registry.clone(),
        configuration.clone(),
    );
    wait_transfer_snapshot(&mut updates, |s| {
        s.tasks.len() == 3 && s.tasks.iter().all(|t| t.state == State::Paused)
    })
    .await?;
    assert!(!directory.path().join("one").exists());
    restored.action(id, Action::Resume)?;
    wait_transfer_snapshot(&mut updates, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == id && t.state == State::Complete)
    })
    .await?;
    assert_eq!(
        tokio::fs::read(directory.path().join("one")).await?,
        vec![42; 4 * 1024 * 1024]
    );
    let mut changed = configuration.clone();
    changed.connections[0].host = "different.example".into();
    restored.configure(changed)?;
    restored.action(second, Action::Resume)?;
    let snapshot = wait_transfer_snapshot(&mut updates, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == second && t.state == State::Failed)
    })
    .await?;
    assert!(
        snapshot
            .tasks
            .iter()
            .find(|t| t.id == second)
            .and_then(|t| t.error.as_ref())
            .is_some_and(|e| e.contains("配置已变化"))
    );
    restored.configure(configuration)?;
    restored.action(second, Action::Cancel)?;
    restored.action(third, Action::Cancel)?;
    wait_transfer_snapshot(&mut updates, |s| {
        s.tasks
            .iter()
            .filter(|t| t.state == State::Canceled)
            .count()
            == 2
    })
    .await?;
    assert!(!directory.path().join("two").exists());
    assert!(
        !directory
            .path()
            .join(format!(".nutty-{second}.part"))
            .exists()
    );
    // A failed request must retry through a fresh SFTP session, then publish exactly once.
    server.files.fail_reads(1);
    restored.enqueue(
        connection.id,
        directory.path().join("retry"),
        "/source.txt".into(),
        Direction::Download,
    )?;
    wait_transfer_snapshot(&mut updates, |s| {
        s.tasks.iter().any(|t| t.state == State::Retrying)
    })
    .await?;
    wait_transfer_snapshot(&mut updates, |s| {
        s.tasks
            .iter()
            .filter(|t| t.state == State::Complete)
            .count()
            == 2
    })
    .await?;
    assert_eq!(
        tokio::fs::read(directory.path().join("retry")).await?,
        b"remote-source\n"
    );
    restored.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn pending_authentication_does_not_block_an_unrelated_connection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = TestServer::start().await?;
    keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        server.address.port(),
        &server.key,
        directory.path().join("known_hosts"),
    )?;
    let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
    let path = directory.path().join("identity");
    std::fs::write(
        &path,
        key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
    )?;
    let mut interactive = connection(&server, path.clone(), 9910, None);
    interactive.auth = Authentication::Interactive;
    let independent = connection(&server, path, 9911, None);
    let (prompts, _) = async_channel::bounded(1);
    let (auth, receiver) = async_channel::bounded(1);
    let registry = Arc::new(Registry::new(directory.path(), prompts).with_authentication(auth));
    let waiting_registry = registry.clone();
    let task = tokio::spawn(async move {
        waiting_registry
            .connect(interactive.id, &[interactive])
            .await
    });
    let prompt = tokio::time::timeout(Duration::from_secs(2), receiver.recv()).await??;
    let node = tokio::time::timeout(
        Duration::from_secs(2),
        registry.connect(independent.id, &[independent]),
    )
    .await??;
    assert!(!node.handle().await.is_closed());
    let _ = prompt.answer.send(None);
    assert!(task.await?.is_err());
    Ok(())
}

async fn wait_for_integrated_prompt(terminal: &Terminal) -> Result<u64> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let revision = terminal.engine.snapshot().automation_revision;
            if terminal.engine.automatic_command_ready(revision) {
                return revision;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("no integrated prompt: {}", terminal.engine.recent_output()))
}

#[tokio::test]
async fn native_remote_shells_capture_typed_history_and_allow_safe_automation() -> Result<()> {
    for shell in ["bash", "sh", "dash", "zsh"] {
        if !Path::new(&format!("/bin/{shell}")).exists() {
            continue;
        }
        for route in ["direct", "jump", "gateway"] {
            let gateway = route == "gateway";
            let directory = tempfile::tempdir()?;
            let server =
                TestServer::start_with_terminal(TerminalFixture::Native { shell, gateway }).await?;
            let jump = if route == "jump" {
                Some(TestServer::start().await?)
            } else {
                None
            };
            let key = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
            let key_path = directory.path().join("identity");
            std::fs::write(
                &key_path,
                key.to_openssh(keys::ssh_key::LineEnding::LF)?.as_bytes(),
            )?;
            for host in std::iter::once(&server).chain(jump.iter()) {
                keys::known_hosts::learn_known_hosts_path(
                    "127.0.0.1",
                    host.address.port(),
                    &host.key,
                    directory.path().join("known_hosts"),
                )?;
            }
            let parent = jump
                .as_ref()
                .map(|server| connection(server, key_path.clone(), 9990, None));
            let connection = connection(&server, key_path, 9991, parent.as_ref().map(|c| c.id));
            let configuration: Vec<_> = parent.into_iter().chain([connection.clone()]).collect();
            let (prompts, _) = async_channel::bounded(1);
            let registry = Arc::new(Registry::new(directory.path(), prompts));
            let node = registry.connect(connection.id, &configuration).await?;
            // Turning off the directory display must not turn off history or automation.
            let terminal = node
                .terminal(&Preferences {
                    track_directory: route != "jump",
                    ..Default::default()
                })
                .await?;
            if gateway {
                wait_for_text(&terminal, "Select target [1]:").await?;
                terminal.engine.resize(crate::terminal::TermSize {
                    columns: 40,
                    rows: 30,
                })?;
                terminal.engine.input(b"1\r".to_vec())?;
            }
            wait_for_integrated_prompt(&terminal)
                .await
                .with_context(|| format!("{shell}/{route} bootstrap"))?;
            if gateway {
                assert_eq!(
                    terminal.engine.environment().operating_system,
                    crate::session_environment::Environment::local(Path::new(&format!(
                        "/bin/{shell}"
                    )))
                    .operating_system,
                    "gateway target platform must come from the current terminal channel"
                );
            }
            ensure!(
                terminal.engine.command_history().is_empty(),
                "bootstrap leaked into history"
            );
            ensure!(
                !["nutty-cwd", "__nutty_", "nutty-bootstrap", "eval "]
                    .iter()
                    .any(|text| terminal.engine.recent_output().contains(text)),
                "bootstrap echo visible in {shell}/{route}: {}",
                terminal.engine.recent_output()
            );
            for word in ["first", "second"] {
                let command = format!("printf '%s_%s\\n' typed {word}");
                terminal.engine.input(command.as_bytes().to_vec())?;
                wait_for_text(&terminal, &command).await?;
                terminal.engine.input(b"\r".to_vec())?;
                wait_for_integrated_prompt(&terminal)
                    .await
                    .with_context(|| format!("{shell}/{route} typed"))?;
                ensure!(
                    terminal
                        .engine
                        .recent_output()
                        .lines()
                        .any(|line| line == format!("typed_{word}")),
                    "typed command did not execute in {shell}/{route}"
                );
                ensure!(
                    terminal
                        .engine
                        .command_history()
                        .iter()
                        .any(|h| h.command == command),
                    "typed command missing for {shell}/{route}"
                );
            }
            let revision = wait_for_integrated_prompt(&terminal).await?;
            let command = nutty_core::command::automatic_read_only(
                "uname -srm; printf 'shell=%s\\n' \"$0\"; pwd; ls -la | head -n 40",
            )
            .context("read-only command")?;
            terminal.engine.send_automatic_command(&command, revision)?;
            wait_for_integrated_prompt(&terminal)
                .await
                .with_context(|| format!("{shell}/{route} automatic"))?;
            let output = terminal.engine.recent_output();
            ensure!(
                output.lines().any(|line| line.starts_with("shell=")),
                "printf shell diagnostic did not execute in {shell}/{route}: {output}"
            );
            let directory_printed = terminal.engine.snapshot().directory.map_or_else(
                || {
                    output
                        .lines()
                        .any(|line| line.starts_with('/') && Path::new(line).is_dir())
                },
                |directory| output.replace('\n', "").contains(&directory),
            );
            ensure!(
                directory_printed,
                "automatic command did not produce a directory in {shell}/{route}"
            );
            ensure!(
                terminal.engine.command_history().len() == 3,
                "expected exactly two typed commands and one automatic command in {shell}/{route}"
            );
            registry.disconnect(node.connection.id).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn terminal_start_requires_pty_and_shell_acceptance_and_has_a_deadline() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(12), async {
        for (mode, expected) in [
            (TerminalFixture::RejectPty, "PTY"),
            (TerminalFixture::RejectShell, "Shell"),
            (TerminalFixture::SilentShell, "超时"),
        ] {
            let directory = tempfile::tempdir()?;
            let server = TestServer::start_with_terminal(mode).await?;
            let identity = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
            let key = directory.path().join("identity");
            std::fs::write(
                &key,
                identity
                    .to_openssh(keys::ssh_key::LineEnding::LF)?
                    .as_bytes(),
            )?;
            let mut config = connection(&server, key, 500, None);
            config.ssh.connect_timeout_seconds = 1;
            let (prompts, receive) = async_channel::bounded::<HostPrompt>(1);
            let approve = tokio::spawn(async move {
                let prompt = receive.recv().await.expect("fixture identity prompt");
                let _ = prompt.answer.send(true);
            });
            let registry = Arc::new(Registry::new(directory.path(), prompts));
            let node = registry.connect(config.id, &[config]).await?;
            approve.await?;
            let result = node.terminal(&Preferences::default()).await;
            assert!(
                result.is_err(),
                "unaccepted shell must never create a successful terminal"
            );
            let error = format!("{:#}", result.err().expect("failed startup"));
            assert!(
                error.contains(expected),
                "startup error must name failed phase: {error}"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("terminal startup failed to respect its timeout")?
}

#[tokio::test]
async fn last_terminal_and_forward_release_child_and_jump_nodes() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir()?;
        let identity = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key = directory.path().join("identity");
        std::fs::write(
            &key,
            identity
                .to_openssh(keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        let jump = TestServer::start().await?;
        let server = TestServer::start().await?;
        let root = connection(&jump, key.clone(), 510, None);
        let target = connection(&server, key, 511, Some(root.id));
        let (prompts, receive) = async_channel::bounded::<HostPrompt>(2);
        let approve = tokio::spawn(async move {
            for _ in 0..2 {
                let prompt = receive.recv().await.expect("fixture identity prompt");
                let _ = prompt.answer.send(true);
            }
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry.connect(target.id, &[root, target]).await?;
        approve.await?;
        let child = Arc::downgrade(&node);
        let parent = Arc::downgrade(node._parent.as_ref().expect("jump parent"));
        let first = node.terminal(&Preferences::default()).await?;
        let second = node.terminal(&Preferences::default()).await?;
        wait_for_text(&first, "shell-banner").await?;
        let forwarding = node.forward("127.0.0.1:0".parse()?, None).await?;
        drop(node);
        drop(first);
        second.engine.input(b"second-still-live".to_vec())?;
        wait_for_text(&second, "second-still-live").await?;
        drop(second);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            child.upgrade().is_some(),
            "forward owns its route after the last tab closes"
        );
        drop(forwarding);
        tokio::time::timeout(Duration::from_secs(2), async {
            while child.upgrade().is_some() || parent.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("unused SSH and jump nodes retained after last owner closed")?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn canceled_shell_start_closes_its_channel_without_disconnect() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(6), async {
        let directory = tempfile::tempdir()?;
        let server = TestServer::start_with_terminal(TerminalFixture::SilentShell).await?;
        let identity = keys::PrivateKey::random(&mut rand::rng(), keys::Algorithm::Ed25519)?;
        let key = directory.path().join("identity");
        std::fs::write(
            &key,
            identity
                .to_openssh(keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        let config = connection(&server, key, 520, None);
        let (prompts, receive) = async_channel::bounded::<HostPrompt>(1);
        let approve = tokio::spawn(async move {
            let prompt = receive.recv().await.expect("fixture identity prompt");
            let _ = prompt.answer.send(true);
        });
        let registry = Arc::new(Registry::new(directory.path(), prompts));
        let node = registry.connect(config.id, &[config]).await?;
        approve.await?;
        let canceled = tokio::time::timeout(
            Duration::from_millis(100),
            node.terminal(&Preferences::default()),
        )
        .await;
        assert!(
            canceled.is_err(),
            "silent shell must be pending when the caller cancels"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while server
                .closed_channels
                .load(std::sync::atomic::Ordering::SeqCst)
                < 1
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("canceled shell channel must be closed")?;
        assert!(!node.cancel.is_cancelled());
        assert!(!node.handle().await.is_closed());
        assert!(
            !node.list(".").await?.is_empty(),
            "independent SFTP must stay usable"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
