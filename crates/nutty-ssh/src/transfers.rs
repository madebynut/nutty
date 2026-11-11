//! Persistent, bounded SFTP task scheduling and restart-safe transfer recovery.
mod endpoint;
mod job;
use crate::remote::{Node, Registry};
use anyhow::{Context as _, Result, ensure};
use nutty_core::{config::Connection, network::Proxy};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TASKS: usize = 128;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum State {
    Queued,
    Preparing,
    Running,
    Verifying,
    Publishing,
    Pausing,
    Paused,
    Retrying,
    Failed,
    Canceling,
    Canceled,
    Complete,
    Skipped,
}
impl State {
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Preparing
                | Self::Running
                | Self::Verifying
                | Self::Publishing
                | Self::Pausing
                | Self::Canceling
        )
    }
    pub fn pending(self) -> bool {
        self.active() || matches!(self, Self::Queued | Self::Retrying)
    }
    pub fn label(self) -> &'static str {
        nutty_core::i18n::text(match self {
            Self::Queued => "排队中",
            Self::Preparing => "扫描目录",
            Self::Running => "传输中",
            Self::Verifying => "校验续传内容",
            Self::Publishing => "正在发布",
            Self::Pausing => "正在暂停",
            Self::Paused => "已暂停",
            Self::Retrying => "等待重试",
            Self::Failed => "传输失败",
            Self::Canceling => "清理分片",
            Self::Canceled => "已取消",
            Self::Complete => "完成",
            Self::Skipped => "已跳过",
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Request {
    connection: Uuid,
    local: PathBuf,
    remote: String,
    direction: Direction,
    route: Vec<Connection>,
    proxies: Vec<Proxy>,
    limit: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Job {
    version: u8,
    #[serde(default)]
    conflict: Option<endpoint::Fingerprint>,
    #[serde(default)]
    resolution: Resolution,
    #[serde(default)]
    replacement: Option<endpoint::Fingerprint>,
    #[serde(default)]
    skip_requested: bool,
    #[serde(default)]
    restart_requested: bool,
    #[serde(default)]
    cleanup_requested: bool,
    id: Uuid,
    request: Request,
    state: State,
    error: Option<String>,
    bytes: u64,
    total: u64,
    completed: u64,
    entries: u64,
    cursor: u64,
    source: Option<String>,
    target: Option<String>,
    manifest_ready: bool,
    publishing: bool,
    attempt: u8,
}
impl Job {
    fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "不支持此传输记录版本");
        ensure!(
            self.resolution != Resolution::ReplaceFile
                || self
                    .replacement
                    .as_ref()
                    .is_some_and(
                        |fingerprint| fingerprint.metadata.kind == endpoint::Kind::File
                            && fingerprint.sha256.is_some()
                    ),
            "替换记录缺少原文件校验信息"
        );
        ensure!(
            self.request.local.is_absolute()
                && self.request.local.to_str().is_some_and(|p| p.len() <= 4096)
                && self.request.remote.len() <= 4096
                && !self.request.remote.contains('\0'),
            "传输路径无效"
        );
        ensure!(
            self.request.route.len() <= 32
                && self.request.proxies.len() <= 32
                && self.entries <= 100_000
                && self.cursor <= 64 * 1024 * 1024,
            "传输记录超过限制"
        );
        ensure!(
            self.source.is_some() == self.target.is_some()
                && (!self.manifest_ready || self.source.is_some()),
            "传输记录状态无效"
        );
        for (path, local) in [
            (&self.source, self.request.direction == Direction::Upload),
            (&self.target, self.request.direction == Direction::Download),
        ] {
            let Some(path) = path else {
                continue;
            };
            ensure!(
                (if local {
                    std::path::Path::new(path).is_absolute()
                } else {
                    path.starts_with('/')
                }) && path.len() <= 8192
                    && !path.contains('\0'),
                "传输记录路径无效"
            );
        }
        Ok(())
    }
    fn summary(&self) -> Task {
        Task {
            id: self.id,
            name: self.request.remote.clone(),
            host: self
                .request
                .route
                .last()
                .map(|c| format!("{}@{}:{}", c.user, c.host, c.port))
                .unwrap_or_default(),
            direction: self.request.direction,
            state: self.state,
            bytes: self.bytes,
            total: self.total,
            completed: self.completed,
            entries: self.entries,
            error: self.error.clone(),
            conflict: self.conflict.is_some(),
            replaceable: self
                .conflict
                .as_ref()
                .is_some_and(|f| f.metadata.kind == endpoint::Kind::File && f.sha256.is_some()),
            target: self.target.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub direction: Direction,
    pub state: State,
    pub bytes: u64,
    pub total: u64,
    pub completed: u64,
    pub entries: u64,
    pub error: Option<String>,
    pub conflict: bool,
    pub replaceable: bool,
    pub target: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Debug)]
struct Progress {
    bytes: u64,
    total: u64,
    completed: u64,
    entries: u64,
    state: State,
}
#[derive(Clone)]
pub struct Configuration {
    pub connections: Vec<Connection>,
    pub proxies: Vec<Proxy>,
    pub concurrency: usize,
    pub limit: u64,
}
#[derive(Clone, Copy)]
pub enum Action {
    Pause,
    Resume,
    Cancel,
    Remove,
    Restart,
    KeepBoth,
    ReplaceFile,
    Skip,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
enum Resolution {
    #[default]
    Ask,
    KeepBoth,
    ReplaceFile,
}
enum Command {
    Enqueue {
        connection: Uuid,
        local: PathBuf,
        remote: String,
        direction: Direction,
    },
    Action(Uuid, Action),
    Configure(Configuration),
    Shutdown(oneshot::Sender<()>),
}
#[derive(Clone)]
pub struct Queue {
    commands: mpsc::Sender<Command>,
    stop: CancellationToken,
}
impl Queue {
    /// Starts an actor on the caller's Tokio runtime. Restored work stays paused.
    pub fn start(
        directory: PathBuf,
        registry: Arc<Registry>,
        configuration: Configuration,
    ) -> (Self, watch::Receiver<Snapshot>) {
        let (commands, receiver) = mpsc::channel(64);
        let (updates, snapshots) = watch::channel(Snapshot::default());
        let stop = CancellationToken::new();
        let stopping = stop.clone();
        tokio::spawn(async move {
            if let Err(error) = run(
                directory,
                registry,
                configuration,
                receiver,
                updates.clone(),
                stopping,
            )
            .await
            {
                updates.send_replace(Snapshot {
                    tasks: Vec::new(),
                    error: Some(format!("传输队列不可用：{error:#}")),
                });
            }
        });
        (Self { commands, stop }, snapshots)
    }
    pub fn enqueue(
        &self,
        connection: Uuid,
        local: PathBuf,
        remote: String,
        direction: Direction,
    ) -> Result<()> {
        self.commands
            .try_send(Command::Enqueue {
                connection,
                local,
                remote,
                direction,
            })
            .context("传输队列忙，请稍后重试")?;
        Ok(())
    }
    pub fn action(&self, id: Uuid, action: Action) -> Result<()> {
        self.commands
            .try_send(Command::Action(id, action))
            .context("传输队列忙，请稍后重试")?;
        Ok(())
    }
    pub fn configure(&self, configuration: Configuration) -> Result<()> {
        self.commands
            .try_send(Command::Configure(configuration))
            .context("传输队列忙，请稍后重试")?;
        Ok(())
    }
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
    pub fn stop(&self) {
        self.stop.cancel();
    }
}
struct Active {
    cancel: CancellationToken,
    progress: watch::Receiver<Progress>,
    requested: Option<State>,
}
async fn run(
    directory: PathBuf,
    registry: Arc<Registry>,
    mut configuration: Configuration,
    mut commands: mpsc::Receiver<Command>,
    updates: watch::Sender<Snapshot>,
    stop: CancellationToken,
) -> Result<()> {
    let root = directory.join("transfers");
    tokio::fs::create_dir_all(&root).await?;
    ensure!(
        tokio::fs::symlink_metadata(&root).await?.is_dir(),
        "传输存储目录不能是符号链接"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let mut jobs = Vec::new();
    let mut errors = Vec::new();
    let mut directories = tokio::fs::read_dir(&root).await?;
    let mut scanned = 0;
    while let Some(entry) = directories.next_entry().await? {
        scanned += 1;
        ensure!(scanned <= 1024, "传输存储目录项过多，请检查存储目录");
        if Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err()
            || !entry.file_type().await?.is_dir()
        {
            continue;
        }
        if jobs.len() >= MAX_TASKS {
            errors.push("恢复任务超过上限，未加载剩余任务".into());
            break;
        }
        match job::read_job(&entry.path().join("task.json")).await {
            Ok(mut j) => {
                if j.folder(&root) != entry.path() {
                    errors.push("任务目录身份不匹配".into());
                    continue;
                }
                if j.state.pending() {
                    j.state = State::Paused;
                    j.error = Some("上次传输被中断，可校验后继续".into());
                    j.save(&root).await?;
                }
                jobs.push(j);
            }
            Err(e) => {
                if errors.len() < 8 {
                    errors.push(format!("无法恢复传输记录：{e:#}"));
                }
            }
        }
    }
    let mut error = (!errors.is_empty()).then(|| errors.join("；"));
    let mut active: HashMap<Uuid, Active> = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();
    let mut retries = HashMap::new();
    let mut shutdown: Option<oneshot::Sender<()>> = None;
    let mut stopping = false;
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if !stopping {
            for j in &mut jobs {
                if active.len() >= configuration.concurrency.clamp(1, 8) {
                    break;
                }
                if !matches!(j.state, State::Queued | State::Retrying | State::Canceling)
                    || active.contains_key(&j.id)
                {
                    continue;
                }
                if retries
                    .get(&j.id)
                    .is_some_and(|deadline| tokio::time::Instant::now() < *deadline)
                {
                    continue;
                }
                retries.remove(&j.id);
                let cleanup = j.cleanup_requested || j.state == State::Canceling;
                let offline_cleanup = cleanup
                    && (j.target.is_none() || j.request.direction == Direction::Download)
                    && !j.publishing;
                if let Err(e) = if offline_cleanup {
                    Ok(())
                } else {
                    verify_configuration(j, &configuration)
                } {
                    j.state = State::Failed;
                    j.error = Some(e.to_string());
                    j.save(&root).await?;
                    continue;
                }
                j.state = if cleanup {
                    State::Canceling
                } else {
                    State::Preparing
                };
                j.error = None;
                j.save(&root).await?;
                let (progress, receiver) = watch::channel(Progress {
                    bytes: j.bytes,
                    total: j.total,
                    completed: j.completed,
                    entries: j.entries,
                    state: j.state,
                });
                let cancel = CancellationToken::new();
                active.insert(
                    j.id,
                    Active {
                        cancel: cancel.clone(),
                        progress: receiver,
                        requested: None,
                    },
                );
                let j = j.clone();
                let root = root.clone();
                let registry = registry.clone();
                workers
                    .spawn(async move { work(j, root, registry, progress, cancel, cleanup).await });
            }
        }
        let tasks = jobs
            .iter()
            .map(|j| {
                let mut t = j.summary();
                if let Some(a) = active.get(&j.id) {
                    let p = *a.progress.borrow();
                    t.bytes = p.bytes;
                    t.total = p.total;
                    t.completed = p.completed;
                    t.entries = p.entries;
                    t.state = a.requested.unwrap_or(p.state);
                }
                t
            })
            .collect();
        let snapshot = Snapshot {
            tasks,
            error: error.clone(),
        };
        updates.send_if_modified(|previous| {
            if *previous == snapshot {
                false
            } else {
                *previous = snapshot;
                true
            }
        });
        if stopping && active.is_empty() {
            if let Some(answer) = shutdown.take() {
                let _ = answer.send(());
            }
            break;
        }
        tokio::select! {
            biased;
            Some(result)=workers.join_next(),if !workers.is_empty()=>{
                match result {
                    Ok((mut j,result))=>{
                        let request=active.remove(&j.id).and_then(|a|a.requested);
                        if request==Some(State::Canceling) {j.cleanup_requested=true;j.restart_requested=false;}
                        if let Err(e)=result {
                            let transient=retryable(&e);j.error=Some(format!("{e:#}").chars().take(2048).collect());
                            if transient && !stopping && request.is_none() && j.attempt<3 {j.attempt+=1;j.state=State::Retrying;retries.insert(j.id,tokio::time::Instant::now()+Duration::from_secs(1<<j.attempt));}else{j.state=State::Failed;}
                        }
                        if j.state!=State::Complete {
                            if stopping || request==Some(State::Pausing){j.state=State::Paused;}
                            else if request==Some(State::Canceling){j.cleanup_requested=true;j.state=State::Canceling;}
                        }
                        if let Err(e)=j.save(&root).await{j.state=State::Failed;j.error=Some(format!("无法保存传输状态：{e:#}"));}
                        if let Some(existing)=jobs.iter_mut().find(|old|old.id==j.id){*existing=j;}
                    },
                    Err(e)=>{error=Some(format!("传输工作线程异常：{e}"));stopping=true;for a in active.values(){a.cancel.cancel();}active.clear();}
                }
            },
            command=commands.recv(),if !stopping=>match command {
                Some(Command::Configure(c))=>configuration=c,
                Some(Command::Shutdown(answer))=>{shutdown=Some(answer);stopping=true;for a in active.values(){a.cancel.cancel();}for j in &mut jobs{if !active.contains_key(&j.id) && matches!(j.state,State::Queued|State::Retrying|State::Canceling){j.state=State::Paused;j.save(&root).await?;}}},
                Some(Command::Enqueue{connection,local,remote,direction})=>{
                    let result=async {
                        ensure!(jobs.len()<MAX_TASKS,"传输队列最多 128 个任务，请移除已完成记录");
                        let route=crate::remote::route(connection,&configuration.connections)?.into_iter().cloned().collect::<Vec<_>>();
                        let proxies=relevant_proxies(&route,&configuration.proxies);
                        let j=Job{version:1,conflict:None,resolution:Resolution::Ask,replacement:None,skip_requested:false,restart_requested:false,cleanup_requested:false,id:Uuid::new_v4(),request:Request{connection,local,remote,direction,route,proxies,limit:configuration.limit},state:State::Queued,error:None,bytes:0,total:0,completed:0,entries:0,cursor:0,source:None,target:None,manifest_ready:false,publishing:false,attempt:0};j.validate()?;
                        ensure!(!jobs.iter().any(|old|!matches!(old.state,State::Complete|State::Canceled|State::Skipped) && same_destination(old,&j)),"同一目标已有传输任务");
                        tokio::fs::create_dir(j.folder(&root)).await?;j.save(&root).await?;jobs.push(j);Ok::<_,anyhow::Error>(())
                    }.await;error=result.err().map(|e|format!("{e:#}"));
                },
                Some(Command::Action(id,action))=>{
                    if let Some(index)=jobs.iter().position(|j|j.id==id){
                        let j=&mut jobs[index];
                retries.remove(&j.id);
                        if let Some(a)=active.get_mut(&id){match action{Action::Pause=>{a.requested=Some(State::Pausing);a.cancel.cancel();},Action::Cancel=>{a.requested=Some(State::Canceling);a.cancel.cancel();},_=>{}}}
                        else {match action {
                            Action::KeepBoth | Action::ReplaceFile if j.state == State::Failed && j.conflict.is_some() => {
                                if matches!(action, Action::KeepBoth) || j.conflict.as_ref().is_some_and(|f| f.sha256.is_some()) {
                                    j.resolution = if matches!(action, Action::KeepBoth) { Resolution::KeepBoth } else { Resolution::ReplaceFile };
                                    j.replacement = if j.resolution == Resolution::ReplaceFile { j.conflict.take() } else { None };
                                    j.conflict = None;
                                    j.publishing = false;
                                    j.state = State::Queued;
                                    j.attempt = 0;
                                    j.save(&root).await?;
                                }
                            },
                            Action::Skip if j.state == State::Failed && j.conflict.is_some() => {
                                j.publishing=false; j.skip_requested=true; j.cleanup_requested=true;
                                j.state=State::Canceling; j.save(&root).await?;
                            },
                            Action::Pause if j.state.pending()=>{j.state=State::Paused;j.save(&root).await?;},
                            Action::Resume if matches!(j.state,State::Paused|State::Failed)=>{j.state=State::Queued;j.attempt=0;j.request.limit=configuration.limit;retries.remove(&id);j.save(&root).await?;},
                            Action::Restart if matches!(j.state,State::Paused|State::Failed)=>{j.restart_requested=true;j.cleanup_requested=true;j.state=State::Canceling;j.save(&root).await?;},
                            Action::Cancel if !matches!(j.state,State::Complete|State::Canceled|State::Skipped)=>{j.restart_requested=false;j.cleanup_requested=true;j.state=State::Canceling;j.save(&root).await?;},
                            Action::Remove if matches!(j.state,State::Complete|State::Canceled|State::Skipped)=>{tokio::fs::remove_dir_all(j.folder(&root)).await?;jobs.remove(index);},_=>{}
                        }}
                    }
                },
                None=>{stopping=true;for a in active.values(){a.cancel.cancel();}}
            },
            ()=stop.cancelled(),if !stopping=>{stopping=true;for a in active.values(){a.cancel.cancel();}},
            _=tick.tick(),if !active.is_empty() || !retries.is_empty()=>{}
        }
    }
    Ok(())
}
fn relevant_proxies(route: &[Connection], proxies: &[Proxy]) -> Vec<Proxy> {
    proxies
        .iter()
        .filter(|p| route.iter().any(|c| c.proxy == Some(p.id)))
        .cloned()
        .collect()
}
fn verify_configuration(j: &Job, c: &Configuration) -> Result<()> {
    let route = crate::remote::route(j.request.connection, &c.connections)?
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        route == j.request.route
            && serde_json::to_vec(&relevant_proxies(&route, &c.proxies))?
                == serde_json::to_vec(&j.request.proxies)?,
        "连接或跳板配置已变化，请新建传输，旧任务不会连接其他主机"
    );
    Ok(())
}
fn same_destination(a: &Job, b: &Job) -> bool {
    a.request.direction == b.request.direction
        && match a.request.direction {
            Direction::Upload => {
                a.request.connection == b.request.connection && a.request.remote == b.request.remote
            }
            Direction::Download => a.request.local == b.request.local,
        }
}
fn retryable(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.is::<tokio::time::error::Elapsed>()
            || e.downcast_ref::<russh::Error>().is_some_and(|e| {
                matches!(
                    e,
                    russh::Error::Disconnect
                        | russh::Error::HUP
                        | russh::Error::ConnectionTimeout
                        | russh::Error::KeepaliveTimeout
                        | russh::Error::InactivityTimeout
                        | russh::Error::SendError
                )
            })
            || e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::NotConnected
                )
            })
            || e.downcast_ref::<russh_sftp::client::error::Error>()
                .is_some_and(|e| match e {
                    russh_sftp::client::error::Error::Timeout
                    | russh_sftp::client::error::Error::IO(_) => true,
                    russh_sftp::client::error::Error::UnexpectedBehavior(message) => {
                        message.starts_with("RecvError:") || message.starts_with("SendError:")
                    }
                    russh_sftp::client::error::Error::Status(status) => matches!(
                        status.status_code,
                        russh_sftp::protocol::StatusCode::ConnectionLost
                            | russh_sftp::protocol::StatusCode::NoConnection
                    ),
                    _ => false,
                })
    })
}
async fn work(
    mut j: Job,
    root: PathBuf,
    registry: Arc<Registry>,
    progress: watch::Sender<Progress>,
    cancel: CancellationToken,
    cleanup: bool,
) -> (Job, Result<()>) {
    let result=async {
        if cleanup && !j.publishing && (j.target.is_none() || j.request.direction == Direction::Download) {
            if j.target.is_some() { tokio::select! { biased; () = cancel.cancelled() => { j.state=State::Paused; return Ok(()); }, result = j.discard(&endpoint::Endpoint::Local) => result? } }
            finish_cleanup(&mut j);
            return Ok(());
        }
        let connect=async {let node=registry.connect_using(j.request.connection,&j.request.route,&j.request.proxies).await?;let session=node.transfer_session().await?;Ok::<_,anyhow::Error>((node,endpoint::Endpoint::Remote(Arc::new(session))))};
        let (node,remote): (Arc<Node>,_)=tokio::select! {biased;()=cancel.cancelled()=>{j.state=State::Paused;return Ok(());},result=connect=>result?};
        let (source,destination)=match j.request.direction{Direction::Upload=>(endpoint::Endpoint::Local,remote),Direction::Download=>(remote,endpoint::Endpoint::Local)};
        if cleanup && !j.publishing {tokio::select! {biased; () = cancel.cancelled() => { j.state=State::Paused; return Ok(()); }, result=j.discard(&destination)=>result? }finish_cleanup(&mut j);return Ok(());}
        tokio::select! {biased;()=node.cancel.cancelled()=>{j.state=State::Paused;Ok(())},result=j.execute(&root,&source,&destination,&progress,&cancel)=>result}
    }.await;
    (j, result)
}
fn finish_cleanup(job: &mut Job) {
    job.cleanup_requested = false;
    if job.restart_requested {
        job.restart_requested = false;
        job.conflict = None;
        job.replacement = None;
        job.resolution = Resolution::Ask;
        job.skip_requested = false;
        job.source = None;
        job.target = None;
        job.manifest_ready = false;
        job.publishing = false;
        job.cursor = 0;
        job.bytes = 0;
        job.total = 0;
        job.entries = 0;
        job.completed = 0;
        job.attempt = 0;
        job.state = State::Queued;
    } else {
        job.state = if job.skip_requested {
            State::Skipped
        } else {
            State::Canceled
        };
    }
}
#[cfg(test)]
mod tests;
