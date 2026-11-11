//! Persistent pane-owned ACP processes with bounded frames and explicit tool permissions.

pub mod configuration;
use configuration::Configuration;

use agent_client_protocol::{
    Agent as Role, Lines,
    v1::{
        ConnectionTo,
        schema::{
            AuthMethod, AuthenticateRequest, CancelNotification, ClientCapabilities, ContentBlock,
            Implementation, InitializeRequest, NewSessionRequest, NewSessionResponse,
            PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
            RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
            SessionId, SessionNotification, SessionUpdate, TextContent,
        },
    },
};
use anyhow::{Context as _, Result, ensure};
use futures_util::{SinkExt, StreamExt as _};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch};
use tokio_util::{
    codec::{FramedRead, FramedWrite, LinesCodec},
    sync::CancellationToken,
};
use uuid::Uuid;

pub struct Permission {
    pub id: Uuid,
    pub detail: String,
    pub options: Vec<String>,
    pub answer: oneshot::Sender<Option<usize>>,
    pub cancel: CancellationToken,
}
struct Prompt {
    text: String,
    cancel: CancellationToken,
    answer: oneshot::Sender<Result<String>>,
    progress: Option<async_channel::Sender<String>>,
}
enum Operation {
    Prompt(Prompt),
    Configure {
        id: String,
        value: serde_json::Value,
        cancel: CancellationToken,
        answer: oneshot::Sender<Result<Configuration>>,
    },
}
struct ActivePrompt {
    text: String,
    cancel: CancellationToken,
    progress: Option<async_channel::Sender<String>>,
    last: tokio::time::Instant,
}
pub struct Peer {
    sender: async_channel::Sender<Operation>,
    cancel: CancellationToken,
    configuration: watch::Receiver<Configuration>,
    prompted: AtomicBool,
}

#[derive(Debug)]
pub struct LoginRequired;

impl std::fmt::Display for LoginRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Agent 需要登录。请先在本地终端完成登录，再重试连接。")
    }
}

impl std::error::Error for LoginRequired {}

fn protocol_failure(error: agent_client_protocol::Error) -> anyhow::Error {
    if [
        "quota",
        "usage limit",
        "rate limit",
        "credit",
        "resource exhausted",
    ]
    .iter()
    .any(|word| error.message.to_ascii_lowercase().contains(word))
    {
        return anyhow::anyhow!("当前 Agent 模型额度或速率受限，请在模型菜单中切换可用模型后重试");
    }
    if error.code == agent_client_protocol::ErrorCode::AUTH_REQUIRED
        || [
            "not logged in",
            "authentication required",
            "invalid api key",
            "unauthorized",
        ]
        .iter()
        .any(|text| error.message.to_ascii_lowercase().contains(text))
    {
        return LoginRequired.into();
    }
    if agent_client_protocol::is_incoming_transport_closed(&error)
        || agent_client_protocol::is_connection_terminated(&error)
    {
        return anyhow::anyhow!("ACP Agent 进程已退出，请检查程序路径、参数和运行环境");
    }
    if error.message.contains("Missing optional dependency") {
        return anyhow::anyhow!("Codex 缺少平台可执行包，请安装本机 Codex 或配置 CODEX_PATH");
    }
    // Agent-controlled data and stack traces can contain credentials or local
    // content. Report the protocol code instead of printing the response object.
    anyhow::anyhow!(
        "Agent 返回错误（{}），请检查登录、配置和网络",
        error.code.value()
    )
}

async fn initialize(
    connection: &ConnectionTo<Role>,
    agent: &crate::integrations::Agent,
    authentication: Option<&str>,
) -> Result<(SessionId, Configuration)> {
    let startup_timeout =
        if agent.process.args.iter().any(|arg| {
            arg.starts_with("@agentclientprotocol/") || arg.starts_with("@zed-industries/")
        }) {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(30)
        };
    let initialized = tokio::time::timeout(
        startup_timeout,
        connection
            .send_request(
                InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1)
                    .client_capabilities(ClientCapabilities::new().meta(
                        serde_json::Map::from_iter([(
                            "parameterizedModelPicker".into(),
                            serde_json::json!(true),
                        )]),
                    ))
                    .client_info(Implementation::new("nutty", env!("CARGO_PKG_VERSION"))),
            )
            .block_task(),
    )
    .await
    .context("Agent 启动超时，首次使用适配器需要下载，请检查 Node.js、软件源和网络")?
    .map_err(protocol_failure)?;
    ensure!(
        initialized.protocol_version == agent_client_protocol::schema::ProtocolVersion::V1,
        "Agent 的 ACP 协议版本不兼容，请升级 Agent"
    );
    // Cursor needs an explicit handshake. Gemini reuses its configured login method.
    // Claude terminal login and OpenCode's provider setup do not use authenticate.
    if let Some(method) = initialized.auth_methods.iter().find(|method| {
        matches!(method, AuthMethod::Agent(_))
            && (method.id().0.as_ref() == "cursor_login"
                || authentication == Some(method.id().0.as_ref()))
    }) {
        tokio::time::timeout(
            Duration::from_secs(30),
            connection
                .send_request(AuthenticateRequest::new(method.id().clone()))
                .block_task(),
        )
        .await
        .context("Agent 认证超时，请检查登录状态和网络后重试")?
        .map_err(protocol_failure)?;
    }
    let session = tokio::time::timeout(
        Duration::from_secs(30),
        connection
            .send_request(agent_client_protocol::UntypedMessage::new(
                "session/new",
                NewSessionRequest::new(agent.process.directory.clone()),
            )?)
            .block_task(),
    )
    .await
    .context("Agent 创建会话超时，请检查登录状态和工作目录后重试")?
    .map_err(protocol_failure)?;
    let configuration = Configuration::from_response(&session);
    let session: NewSessionResponse =
        serde_json::from_value(session).context("Agent 返回了无效会话")?;
    Ok((session.session_id, configuration))
}

async fn gemini_authentication(
    process: &crate::integrations::Process,
    command: &tokio::process::Command,
) -> Option<String> {
    if !process
        .program
        .file_stem()
        .is_some_and(|name| name == "gemini")
    {
        return None;
    }
    let environment = |name: &str| {
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
            .or_else(|| std::env::var(name).ok())
    };
    if environment("GEMINI_API_KEY").is_some_and(|value| !value.is_empty()) {
        return Some("gemini-api-key".into());
    }
    if environment("GOOGLE_GENAI_USE_VERTEXAI").is_some_and(|value| value == "true" || value == "1")
    {
        return Some("vertex-ai".into());
    }
    let mut paths = vec![process.directory.join(".gemini/settings.json")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".gemini/settings.json"));
    }
    for path in paths {
        if !tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.len() <= 64 * 1024)
        {
            continue;
        }
        if let Ok(bytes) = tokio::fs::read(path).await
            && let Ok(settings) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(method) = settings
                .pointer("/security/auth/selectedType")
                .and_then(serde_json::Value::as_str)
            && ["oauth-personal", "gemini-api-key", "vertex-ai"].contains(&method)
        {
            return Some(method.into());
        }
    }
    None
}

impl Peer {
    pub async fn new(
        agent: crate::integrations::Agent,
        permissions: async_channel::Sender<Permission>,
        cancel: CancellationToken,
        startup_cancel: CancellationToken,
    ) -> Result<Self> {
        let mut command = agent.process.command(agent.id).await?;
        let authentication = gemini_authentication(&agent.process, &command).await;
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let (mut child, group) =
            nutty_core::process_tree::ProcessTree::spawn(&mut command).context("启动 ACP Agent")?;
        let input = child.stdin.take().context("Agent stdin 不可用")?;
        let output = child.stdout.take().context("Agent stdout 不可用")?;
        let (sender, receiver) = async_channel::bounded::<Operation>(2);
        let (configuration, configuration_receiver) = watch::channel(Configuration::default());
        let notification_configuration = configuration.clone();
        let session_identity = Arc::new(Mutex::new(None::<SessionId>));
        let notification_session = session_identity.clone();
        let (ready, started) = oneshot::channel();
        let lifetime = cancel.child_token();
        let stopping = lifetime.clone();
        let finished_cancel = lifetime.clone();
        let text: Arc<Mutex<Option<ActivePrompt>>> = Arc::new(Mutex::new(None));
        let notification_text = text.clone();
        let output_cancel = stopping.clone();
        let permission_cancel = stopping.clone();
        let permission_prompt = text.clone();
        tokio::spawn(async move {
            let outgoing = <FramedWrite<tokio::process::ChildStdin, LinesCodec> as SinkExt<
                String,
            >>::sink_map_err(
                FramedWrite::new(input, LinesCodec::new_with_max_length(512 * 1024)),
                |error| std::io::Error::other(error.to_string()),
            );
            let incoming = FramedRead::new(output, LinesCodec::new_with_max_length(512 * 1024))
                .map(|result| result.map_err(|error| std::io::Error::other(error.to_string())));
            let running=agent_client_protocol::Client.v1()
                .on_receive_notification(async move |message:agent_client_protocol::UntypedMessage,_cx| {
                    if message.method != "session/update" { return Ok(()); }
                    if !notification_session.lock().expect("ACP session lock poisoned").as_ref().is_some_and(|session| message.params.get("sessionId").and_then(serde_json::Value::as_str) == Some(session.0.as_ref())) { return Ok(()); }
                    let raw_update = &message.params["update"];
                    notification_configuration.send_if_modified(|config| {
                        config.update_notification(raw_update)
                    });
                    if let Ok(notification)=serde_json::from_value::<SessionNotification>(message.params) && let SessionUpdate::AgentMessageChunk(chunk)=notification.update && let ContentBlock::Text(content)=chunk.content {
                        let mut text=notification_text.lock().expect("ACP output lock poisoned");
                        if let Some(active) = text.as_mut() && !active.cancel.is_cancelled() {
                            if active.text.len()+content.text.len()<=256*1024 {
                                active.text.push_str(&content.text);
                                if active.last.elapsed() >= Duration::from_millis(33) {
                                    if let Some(progress) = &active.progress { let _ = progress.try_send(active.text.clone()); }
                                    active.last = tokio::time::Instant::now();
                                }
                            } else { output_cancel.cancel(); }
                        }
                    }Ok(())
                },agent_client_protocol::on_receive_notification!())
                .on_receive_request(async move |request:RequestPermissionRequest,responder,_connection| {
                    let options:Vec<_>=request.options.into_iter().filter(|option|matches!(option.kind,PermissionOptionKind::AllowOnce|PermissionOptionKind::RejectOnce)).take(16).collect();
                    let detail=serde_json::to_string_pretty(&request.tool_call).unwrap_or_default();
                    let (answer,receive)=oneshot::channel();
                    let active_cancel = permission_prompt.lock().expect("ACP output lock poisoned").as_ref().map(|active| active.cancel.clone());
                    let selected=if detail.len()<=64*1024&&!options.is_empty() && let Some(active_cancel) = active_cancel {
                        let permission=Permission{id:Uuid::new_v4(),detail,options:options.iter().map(|option|option.name.clone()).collect(),answer,cancel:active_cancel.clone()};
                        tokio::select!{()=permission_cancel.cancelled()=>None,()=active_cancel.cancelled()=>None,result=async {
                            permissions.send(permission).await.ok()?;
                            tokio::time::timeout(Duration::from_secs(120),receive).await.ok()?.ok()?
                        }=>result}
                    } else { None };
                    let outcome=selected.and_then(|index|options.get(index)).map(|option|RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))).unwrap_or(RequestPermissionOutcome::Cancelled);
                    responder.respond(RequestPermissionResponse::new(outcome))
                },agent_client_protocol::on_receive_request!())
                .connect_with(Lines::new(outgoing,incoming),move |connection:ConnectionTo<Role>|async move {
                    let initialized=tokio::select!{()=stopping.cancelled()=>return Ok(()),result=initialize(&connection, &agent, authentication.as_deref())=>result};
                    let (session, config)=match initialized{Ok(session)=>session,Err(error)=>{let _=ready.send(Err(error.context("ACP 初始化失败")));return Ok(());}};
                    *session_identity.lock().expect("ACP session lock poisoned") = Some(session.clone());
                    configuration.send_replace(config);
                    let _=ready.send(Ok(()));
                    loop {
                        let operation=tokio::select!{()=stopping.cancelled()=>break,result=receiver.recv()=>match result{Ok(operation)=>operation,Err(_)=>break}};
                        let prompt = match operation {
                            Operation::Prompt(prompt) => prompt,
                            Operation::Configure { id, value, cancel, answer } => {
                                let snapshot = configuration.borrow().clone();
                                let result = async {
                                    let setting = snapshot.settings.iter().find(|setting| setting.id == id).context("Agent 配置已变化，请刷新列表")?;
                                    let (method, params) = setting.request(session.0.as_ref(), value.clone())?;
                                    let response = tokio::time::timeout(Duration::from_secs(15), connection.send_request(agent_client_protocol::UntypedMessage::new(method, params)?).block_task()).await.context("Agent 配置切换超时")?.map_err(protocol_failure)?;
                                    configuration.send_modify(|config| {
                                        if method == "session/set_config_option" { config.update(&response); }
                                        else { config.set_current(&id, value.clone()); }
                                    });
                                    Ok(configuration.borrow().clone())
                                };
                                let result = tokio::select! { ()=cancel.cancelled()=>Err(anyhow::anyhow!("Agent 配置切换已取消")), ()=stopping.cancelled()=>Err(anyhow::anyhow!("会话已结束")), result=result=>result };
                                let _ = answer.send(result);
                                continue;
                            }
                        };
                        *text.lock().expect("ACP output lock poisoned") = Some(ActivePrompt { text: String::new(), cancel: prompt.cancel.child_token(), progress: prompt.progress, last: tokio::time::Instant::now() - Duration::from_secs(1) });
                        let request=connection.send_request(PromptRequest::new(session.clone(),vec![ContentBlock::Text(TextContent::new(prompt.text))])).block_task();
                        let response=tokio::select! {
                            ()=stopping.cancelled()=>None,
                            ()=prompt.cancel.cancelled()=>None,
                            result=tokio::time::timeout(Duration::from_secs(180),request)=>Some(result),
                        };
                        let completed = text.lock().expect("ACP output lock poisoned").take();
                        let text = if let Some(completed) = completed {
                            completed.cancel.cancel();
                            if let Some(progress) = completed.progress { let _ = progress.try_send(completed.text.clone()); }
                            completed.text
                        } else { String::new() };
                        match response{
                            Some(Ok(Ok(_)))=>{let _=prompt.answer.send(Ok(text));},
                            Some(Ok(Err(error)))=>{let closed=agent_client_protocol::is_incoming_transport_closed(&error)||agent_client_protocol::is_connection_terminated(&error);let _=prompt.answer.send(Err(protocol_failure(error).context("ACP 请求失败")));if closed { break; }},
                            Some(Err(_))=>{let _=prompt.answer.send(Err(anyhow::anyhow!("ACP 请求超时")));break;},
                            None=>{let _=connection.send_notification(CancelNotification::new(session.clone()));let _=prompt.answer.send(Err(anyhow::anyhow!("ACP 请求已取消")));break;}
                        }
                    }
                    Ok(())
                });
            let result = running.await;
            finished_cancel.cancel();
            if let Err(error) = result {
                tracing::warn!(code = error.code.value(), "ACP connection ended");
            }
            drop(group);
            let _ = child.start_kill();
            let _ = child.wait().await;
        });
        let peer = Self {
            sender,
            cancel: lifetime,
            configuration: configuration_receiver,
            prompted: AtomicBool::new(false),
        };
        tokio::select! {
            ()=cancel.cancelled()=>anyhow::bail!("会话已结束"),
            ()=startup_cancel.cancelled()=>anyhow::bail!("ACP 请求已取消"),
            result=started=>result.context("ACP Agent 进程未就绪，请检查程序路径、Node.js 和运行环境")??
        };
        Ok(peer)
    }
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed() || self.cancel.is_cancelled()
    }
    pub fn configuration(&self) -> watch::Receiver<Configuration> {
        self.configuration.clone()
    }
    pub fn has_prompted(&self) -> bool {
        self.prompted.load(Ordering::Acquire)
    }
    pub async fn configure(
        &self,
        id: String,
        value: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<Configuration> {
        let (answer, receive) = oneshot::channel();
        tokio::select! {
            ()=cancel.cancelled()=>anyhow::bail!("Agent 配置切换已取消"),
            ()=self.cancel.cancelled()=>anyhow::bail!("会话已结束"),
            result=self.sender.send(Operation::Configure { id, value, cancel:cancel.clone(), answer })=>result.context("ACP 会话已结束")?
        }
        tokio::select! {
            ()=cancel.cancelled()=>anyhow::bail!("Agent 配置切换已取消"),
            ()=self.cancel.cancelled()=>anyhow::bail!("会话已结束"),
            result=receive=>result.context("Agent 配置切换失败")?
        }
    }
    pub async fn prompt(
        &self,
        text: String,
        cancel: CancellationToken,
        progress: Option<async_channel::Sender<String>>,
    ) -> Result<String> {
        ensure!(text.len() <= 256 * 1024, "ACP 上下文过长");
        let (answer, receive) = oneshot::channel();
        self.sender
            .send(Operation::Prompt(Prompt {
                text,
                cancel: cancel.clone(),
                answer,
                progress,
            }))
            .await
            .context("ACP 会话已结束")?;
        self.prompted.store(true, Ordering::Release);
        tokio::select! {()=cancel.cancelled()=>anyhow::bail!("ACP 请求已取消"),()=self.cancel.cancelled()=>anyhow::bail!("ACP 会话已结束"),result=receive=>result.context("ACP 请求结束")?}
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
/// Verifies startup, authentication and session creation without a model prompt.
pub async fn probe(agent: crate::integrations::Agent, cancel: CancellationToken) -> Result<usize> {
    let lifetime = cancel.child_token();
    let _lifetime_guard = lifetime.clone().drop_guard();
    let (permissions, incoming) = async_channel::bounded::<Permission>(8);
    let reject = tokio::spawn(async move {
        while let Ok(permission) = incoming.recv().await {
            let _ = permission.answer.send(None);
        }
    });
    let reject = tokio_util::task::AbortOnDropHandle::new(reject);
    let result = Peer::new(agent, permissions, lifetime.clone(), cancel).await;
    let result = result.map(|peer| {
        peer.configuration()
            .borrow()
            .model()
            .map_or(0, |model| model.choices.len())
    });
    lifetime.cancel();
    reject.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mode: &str, directory: &std::path::Path) -> crate::integrations::Agent {
        crate::integrations::Agent {
            id: Uuid::new_v4(),
            name: "fixture".into(),
            enabled: true,
            process: crate::integrations::Process {
                program: if cfg!(windows) { "python" } else { "python3" }.into(),
                args: vec![
                    format!(
                        "{}/tests/fixtures/protocol-fixtures.py",
                        env!("CARGO_MANIFEST_DIR")
                    ),
                    mode.into(),
                ],
                directory: directory.into(),
                environment: false,
            },
        }
    }

    #[tokio::test]
    async fn availability_probes_real_session_creation_and_reports_login_failure() -> Result<()> {
        let directory = tempfile::tempdir()?;
        assert_eq!(
            probe(
                fixture("acp-config", directory.path()),
                CancellationToken::new()
            )
            .await?,
            4
        );
        assert_eq!(
            probe(
                fixture("acp-cursor", directory.path()),
                CancellationToken::new()
            )
            .await?,
            0
        );
        assert!(
            probe(
                fixture("acp-claude-login", directory.path()),
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            probe(fixture("acp-cursor", directory.path()), cancel)
                .await
                .is_err()
        );
        Ok(())
    }
    #[tokio::test]
    async fn models_and_reasoning_switch_in_one_session_after_quota_failure() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(6), async {
            for mode in ["acp-config", "acp-models-legacy"] {
                let directory = tempfile::tempdir()?;
                let (permissions, _) = async_channel::bounded(2);
                let peer = Peer::new(
                    fixture(mode, directory.path()),
                    permissions,
                    CancellationToken::new(),
                    CancellationToken::new(),
                )
                .await?;
                let configuration = peer.configuration();
                let snapshot = configuration.borrow().clone();
                let model = snapshot.model().context("Agent advertises models")?;
                assert_eq!(model.current, "other");
                let error = peer
                    .prompt("first question".into(), CancellationToken::new(), None)
                    .await
                    .err()
                    .context("fixture quota fails")?;
                assert!(format!("{error:#}").contains("模型菜单"));
                assert!(
                    !peer.is_closed(),
                    "quota errors must retain the Agent session"
                );
                let selected = peer
                    .configure(model.id.clone(), "auto".into(), CancellationToken::new())
                    .await?;
                assert_eq!(selected.model().context("selected model")?.current, "auto");
                if mode == "acp-config" {
                    let error = peer
                        .configure(
                            model.id.clone(),
                            "rejected".into(),
                            CancellationToken::new(),
                        )
                        .await
                        .err()
                        .context("fixture rejects model")?;
                    assert!(!format!("{error:#}").contains("fixture-secret"));
                    assert_eq!(
                        configuration
                            .borrow()
                            .model()
                            .context("failed setting retains model")?
                            .current,
                        "auto"
                    );
                    peer.configure("effort".into(), "low".into(), CancellationToken::new())
                        .await?;
                }
                let reply = peer
                    .prompt("second question".into(), CancellationToken::new(), None)
                    .await?;
                assert_eq!(
                    reply,
                    if mode == "acp-config" {
                        "auto:low"
                    } else {
                        "auto:high"
                    }
                );
                assert!(
                    !configuration.borrow().settings.is_empty(),
                    "another session's notification must be ignored"
                );
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn cursor_authenticates_existing_login_before_creating_a_session() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (permissions, _) = async_channel::bounded(1);
        let peer = tokio::time::timeout(
            Duration::from_secs(3),
            Peer::new(
                fixture("acp-cursor", directory.path()),
                permissions,
                CancellationToken::new(),
                CancellationToken::new(),
            ),
        )
        .await??;
        assert!(!peer.is_closed());
        drop(peer);
        Ok(())
    }

    #[tokio::test]
    async fn claude_uses_terminal_login_without_cursor_authentication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        for mode in ["acp-claude", "acp-claude-login"] {
            let (permissions, _) = async_channel::bounded(1);
            let result = Peer::new(
                fixture(mode, directory.path()),
                permissions,
                CancellationToken::new(),
                CancellationToken::new(),
            )
            .await;
            if mode == "acp-claude" {
                assert!(result.is_ok());
            } else {
                assert!(
                    result
                        .err()
                        .is_some_and(|error| error.is::<LoginRequired>())
                );
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gemini_reuses_configured_auth_before_opening_session() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join(".gemini"))?;
        std::fs::write(
            directory.path().join(".gemini/settings.json"),
            r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        )?;
        let launcher = directory.path().join("gemini");
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexec python3 '{}' acp-gemini\n",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/protocol-fixtures.py"
                )
            ),
        )?;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
        let mut agent = fixture("acp-gemini", directory.path());
        agent.process.program = launcher;
        agent.process.args.clear();
        let mut command = tokio::process::Command::new(&agent.process.program);
        command
            .env("GEMINI_API_KEY", "")
            .env("GOOGLE_GENAI_USE_VERTEXAI", "");
        assert_eq!(
            gemini_authentication(&agent.process, &command)
                .await
                .as_deref(),
            Some("oauth-personal")
        );
        let (permissions, _) = async_channel::bounded(1);
        let peer = Peer::new(
            agent,
            permissions,
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await?;
        assert!(!peer.is_closed());
        Ok(())
    }

    #[tokio::test]
    async fn missing_login_is_actionable_and_never_exposes_protocol_data() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (permissions, _) = async_channel::bounded(1);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            Peer::new(
                fixture("acp-login", directory.path()),
                permissions,
                CancellationToken::new(),
                CancellationToken::new(),
            ),
        )
        .await?;
        let error = result.err().context("login should be required")?;
        assert!(error.downcast_ref::<LoginRequired>().is_some());
        assert!(!format!("{error:#}").contains("fixture-secret"));
        assert!(!format!("{error:#}").contains("-32000"));
        Ok(())
    }

    #[tokio::test]
    async fn stopping_during_agent_startup_cancels_and_reaps_the_process() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("pid");
        let mut agent = fixture("acp-startup-wait", directory.path());
        agent
            .process
            .args
            .push(marker.to_string_lossy().into_owned());
        let (permissions, _) = async_channel::bounded(1);
        let cancel = CancellationToken::new();
        let started = tokio::spawn(Peer::new(
            agent,
            permissions,
            CancellationToken::new(),
            cancel.clone(),
        ));
        let pid: u32 = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(pid) = tokio::fs::read_to_string(&marker).await {
                    break pid.parse();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), started)
                .await??
                .is_err()
        );
        #[cfg(unix)]
        tokio::time::timeout(Duration::from_secs(1), async {
            while nix::sys::signal::kill(nix::unistd::Pid::from_raw(i32::try_from(pid)?), None)
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        #[cfg(not(unix))]
        let _ = pid;
        Ok(())
    }
    #[tokio::test]
    async fn acp_keeps_one_session_and_requires_each_permission() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(6), async {
            let directory = tempfile::tempdir()?;
            let (permissions, receive) = async_channel::bounded(2);
            let cancel = CancellationToken::new();
            let agent = crate::integrations::Agent {
                id: Uuid::new_v4(),
                name: "fixture".into(),
                enabled: true,
                process: crate::integrations::Process {
                    program: if cfg!(windows) { "python" } else { "python3" }.into(),
                    args: vec![
                        format!(
                            "{}/tests/fixtures/protocol-fixtures.py",
                            env!("CARGO_MANIFEST_DIR")
                        ),
                        "acp".into(),
                    ],
                    directory: directory.path().into(),
                    environment: false,
                },
            };
            let peer =
                Peer::new(agent, permissions, cancel.clone(), CancellationToken::new()).await?;
            for (choice, expected) in [
                (Some(0), "Investigating. Decision: allow"),
                (None, "Investigating. Decision: cancelled"),
            ] {
                let (progress, updates) = async_channel::bounded(4);
                let replying = async {
                    assert_eq!(
                        updates.recv().await?,
                        "Investigating. ",
                        "agent text must stream before permission is answered"
                    );
                    let permission: Permission = receive.recv().await?;
                    assert!(permission.detail.contains("pwd"));
                    assert_eq!(permission.options, ["Allow once", "Reject once"]);
                    let _ = permission.answer.send(choice);
                    Ok::<_, anyhow::Error>(())
                };
                let (reply, result) = tokio::join!(
                    peer.prompt(
                        "Investigate.".into(),
                        CancellationToken::new(),
                        Some(progress)
                    ),
                    replying
                );
                result?;
                assert_eq!(reply?, expected);
            }
            let request_cancel = CancellationToken::new();
            let cancelling = async {
                let permission: Permission = receive.recv().await?;
                request_cancel.cancel();
                tokio::time::timeout(Duration::from_secs(1), permission.cancel.cancelled()).await?;
                Ok::<_, anyhow::Error>(())
            };
            let (reply, cancelled) = tokio::join!(
                peer.prompt(
                    "Cancel pending permission".into(),
                    request_cancel.clone(),
                    None
                ),
                cancelling
            );
            cancelled?;
            assert!(reply.is_err());
            cancel.cancel();
            assert!(peer.is_closed());
            assert!(
                peer.prompt("closed".into(), CancellationToken::new(), None)
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("ACP workflow timed out")?
    }
}
