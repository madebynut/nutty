//! Durable transfer checkpoints and bounded, post-order directory manifests.
use super::endpoint::{self, CHUNK, Directory, Endpoint, File, Kind, Metadata};
use super::{Direction, Job, Progress, Resolution, State};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

const MAX_ENTRIES: u64 = 100_000;
const MAX_PLAN: u64 = 64 * 1024 * 1024;
const MAX_LINE: usize = 32 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    relative: String,
    metadata: Metadata,
    finish: bool,
    link: Option<String>,
}
struct CopyPaths<'a> {
    source: &'a str,
    target: &'a str,
    partial: &'a str,
}
impl Entry {
    fn validate(&self) -> Result<()> {
        endpoint::relative(&self.relative)?;
        ensure!(
            self.link
                .as_ref()
                .is_none_or(|l| l.len() <= 4096 && !l.contains('\0')),
            "链接目标无效"
        );
        Ok(())
    }
}

pub(super) async fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 256 * 1024, "传输记录过大");
    let temp = path.with_extension("new");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    let mut file = options.open(&temp).await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(temp, path).await?;
    #[cfg(unix)]
    {
        let parent = path.parent().context("缺少任务目录")?.to_owned();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all()).await??;
    }
    Ok(())
}
pub(super) async fn read_job(path: &Path) -> Result<Job> {
    let meta = tokio::fs::symlink_metadata(path).await?;
    ensure!(meta.is_file() && meta.len() <= 256 * 1024, "传输记录无效");
    let bytes = tokio::fs::read(path).await?;
    let job: Job = serde_json::from_slice(&bytes)?;
    job.validate()?;
    Ok(job)
}
impl Job {
    pub(super) fn folder(&self, root: &Path) -> PathBuf {
        root.join(self.id.to_string())
    }
    pub(super) async fn save(&self, root: &Path) -> Result<()> {
        atomic_json(&self.folder(root).join("task.json"), self).await
    }
    fn source(&self) -> &str {
        self.source.as_deref().expect("resolved transfer source")
    }
    fn target(&self) -> &str {
        self.target.as_deref().expect("resolved transfer target")
    }
    fn stage(&self) -> String {
        let parent = self
            .target()
            .rsplit_once(['/', '\\'])
            .map(|(p, _)| p)
            .unwrap_or_default();
        endpoint::join(parent, &format!(".nutty-{}.part", self.id))
    }
    fn publish_progress(
        &self,
        progress: &tokio::sync::watch::Sender<Progress>,
        bytes: u64,
        phase: State,
    ) {
        progress.send_replace(Progress {
            bytes,
            total: self.total,
            completed: self.completed,
            entries: self.entries,
            state: phase,
        });
    }
    pub(super) async fn execute(
        &mut self,
        root: &Path,
        source: &Endpoint,
        destination: &Endpoint,
        progress: &tokio::sync::watch::Sender<Progress>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.source.is_none() {
            let (src, dst) = match self.request.direction {
                Direction::Upload => (
                    self.request.local.to_str().context("路径不是 UTF-8")?,
                    self.request.remote.as_str(),
                ),
                Direction::Download => (
                    self.request.remote.as_str(),
                    self.request.local.to_str().context("路径不是 UTF-8")?,
                ),
            };
            let resolved_source = if matches!(src.trim_end_matches('/'), "" | "." | "..") {
                source.canonical(src).await?
            } else {
                resolve(source, src).await?
            };
            let resolved_target = resolve(destination, dst).await?;
            // A manifest inside its own source would grow as it is scanned.
            if matches!(source, Endpoint::Local) {
                let storage = tokio::fs::canonicalize(root).await?;
                ensure!(
                    !storage.starts_with(&resolved_source),
                    "源目录包含传输记录目录，请选择其子目录"
                );
                if matches!(destination, Endpoint::Local) {
                    ensure!(
                        !Path::new(&resolved_target).starts_with(&resolved_source),
                        "目标不能位于源目录内"
                    );
                }
            }
            self.source = Some(resolved_source);
            self.target = Some(resolved_target);
            self.save(root).await?;
        }
        if self.publishing {
            return tokio::select! {
                biased;
                () = cancel.cancelled() => { self.state = State::Paused; Ok(()) },
                result = self.reconcile(root, source, destination, progress) => result,
            };
        }
        let preparing = async {
            self.prepare_destination(root, source, destination).await?;
            if !self.manifest_ready {
                self.publish_progress(progress, 0, State::Preparing);
                let plan_path = self.folder(root).join("plan.new");
                let mut plan = tokio::fs::File::create(&plan_path).await?;
                let mut entries = 0;
                let mut bytes = 0;
                let mut total = 0;
                build_plan(
                    source,
                    self.source(),
                    String::new(),
                    &mut plan,
                    &mut entries,
                    &mut bytes,
                    &mut total,
                    0,
                )
                .await?;
                plan.sync_all().await?;
                drop(plan);
                tokio::fs::rename(plan_path, self.folder(root).join("plan.jsonl")).await?;
                self.entries = entries;
                self.total = total;
                self.manifest_ready = true;
                self.save(root).await?;
            }
            self.ensure_stage(destination).await?;
            let mut plan = tokio::io::BufReader::new(
                tokio::fs::File::open(self.folder(root).join("plan.jsonl")).await?,
            );
            plan.seek(std::io::SeekFrom::Start(self.cursor)).await?;
            let mut last_checkpoint = tokio::time::Instant::now();
            while let Some((entry, length)) = read_entry(&mut plan).await? {
                self.publish_progress(progress, self.bytes, State::Running);
                let src = endpoint::join(self.source(), &entry.relative);
                let dst = endpoint::join(&endpoint::join(&self.stage(), "data"), &entry.relative);
                let current = source.metadata(&src).await?.context("源文件已被删除")?;
                ensure!(
                    entry.metadata.unchanged(&current),
                    "源文件已变化，不能继续旧传输"
                );
                match entry.metadata.kind {
                    Kind::Directory => {
                        if entry.finish {
                            destination.permissions(&dst, &entry.metadata).await?;
                        } else if let Some(meta) = destination.metadata(&dst).await? {
                            ensure!(meta.kind == Kind::Directory, "暂存目录被替换");
                        } else {
                            destination.mkdir(&dst).await?;
                        }
                    }
                    Kind::Symlink => {
                        let link = entry.link.as_deref().context("缺少链接目标")?;
                        ensure!(source.read_link(&src).await? == link, "源链接已变化");
                        if destination.metadata(&dst).await?.is_some() {
                            ensure!(destination.read_link(&dst).await? == link, "暂存链接不匹配");
                        } else {
                            destination.symlink(&dst, link).await?;
                        }
                    }
                    Kind::File => {
                        let partial =
                            endpoint::join(&self.stage(), &format!("part-{}", self.cursor));
                        self.copy_file(
                            source,
                            destination,
                            CopyPaths {
                                source: &src,
                                target: &dst,
                                partial: &partial,
                            },
                            &entry.metadata,
                            progress,
                        )
                        .await?;
                        self.bytes = self
                            .bytes
                            .checked_add(entry.metadata.size)
                            .context("传输大小溢出")?;
                    }
                }
                self.cursor += length;
                self.completed += 1;
                if self.completed.is_multiple_of(32)
                    || last_checkpoint.elapsed() >= Duration::from_millis(500)
                {
                    self.save(root).await?;
                    last_checkpoint = tokio::time::Instant::now();
                }
            }
            ensure!(
                self.completed == self.entries && self.bytes == self.total,
                "传输清单不完整"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {biased; ()=cancel.cancelled()=>{self.state=State::Paused;self.save(root).await?;return Ok(());}, result=preparing=>result?}
        // Publication is a bounded commit phase: pause/cancel waits for its known outcome.
        self.publishing = true;
        self.state = State::Publishing;
        self.save(root).await?;
        self.publish_progress(progress, self.bytes, State::Publishing);
        tokio::time::timeout(Duration::from_secs(30), self.publish(source, destination))
            .await
            .context("文件发布超时，可重试核对结果")??;
        self.state = State::Complete;
        self.save(root).await?;
        self.clean_empty_stage(destination).await?;
        Ok(())
    }
    async fn prepare_destination(
        &mut self,
        root: &Path,
        source: &Endpoint,
        destination: &Endpoint,
    ) -> Result<()> {
        if self.resolution == Resolution::KeepBoth {
            let path = self.target().to_owned();
            let (parent, name) = path.rsplit_once(['/', '\\']).context("缺少目标目录")?;
            let (stem, extension) = name
                .rsplit_once('.')
                .filter(|(stem, _)| !stem.is_empty())
                .map(|(stem, ext)| (stem, format!(".{ext}")))
                .unwrap_or((name, String::new()));
            let mut available = None;
            for index in 2..=100 {
                let candidate = endpoint::join(parent, &format!("{stem} ({index}){extension}"));
                if destination.metadata(&candidate).await?.is_none() {
                    available = Some(candidate);
                    break;
                }
            }
            self.target = Some(available.context("同名副本过多，请选择新的名称")?);
            self.resolution = Resolution::Ask;
            self.save(root).await?;
        }
        let found = destination.fingerprint(self.target()).await?;
        if self.resolution == Resolution::ReplaceFile {
            ensure!(
                source
                    .metadata(self.source())
                    .await?
                    .is_some_and(|m| m.kind == Kind::File),
                "目录和链接不能替换普通文件，请选择保留两份"
            );
            if found != self.replacement || found.is_none() {
                self.conflict = found;
                self.resolution = Resolution::Ask;
                self.replacement = None;
                anyhow::bail!("目标在确认后已变化，请重新选择冲突操作");
            }
        } else if found.is_some() {
            self.set_conflict(source, found).await?;
            anyhow::bail!("目标已存在，请选择保留两份、跳过或替换文件");
        }
        Ok(())
    }
    async fn set_conflict(
        &mut self,
        source: &Endpoint,
        mut found: Option<endpoint::Fingerprint>,
    ) -> Result<()> {
        if !source
            .metadata(self.source())
            .await?
            .is_some_and(|m| m.kind == Kind::File)
            && let Some(found) = &mut found
        {
            found.sha256 = None;
        }
        self.conflict = found;
        Ok(())
    }
    async fn publish(&mut self, source: &Endpoint, destination: &Endpoint) -> Result<()> {
        let staged = endpoint::join(&self.stage(), "data");
        let found = destination.fingerprint(self.target()).await?;
        if self.resolution == Resolution::ReplaceFile {
            if found != self.replacement || found.is_none() {
                self.set_conflict(source, found).await?;
                anyhow::bail!("目标在传输期间已变化，原文件未替换");
            }
            if let Err(error) = destination.replace_file(&staged, self.target()).await {
                // Only offer a new resolution after proving that rename did not consume
                // the stage. A lost reply is otherwise reconciled before any new action.
                if destination
                    .metadata(&staged)
                    .await
                    .is_ok_and(|m| m.is_some())
                    && let Ok(found) = destination.fingerprint(self.target()).await
                {
                    self.set_conflict(source, found).await?;
                }
                return Err(error);
            }
        } else {
            if found.is_some() {
                self.set_conflict(source, found).await?;
                anyhow::bail!("目标在传输期间已创建，请选择冲突操作");
            }
            destination.rename(&staged, self.target()).await?;
        }
        self.conflict = None;
        Ok(())
    }
    async fn ensure_stage(&self, destination: &Endpoint) -> Result<()> {
        let stage = self.stage();
        let marker = endpoint::join(&stage, "owner");
        if destination.metadata(&stage).await?.is_none() {
            destination.mkdir(&stage).await?;
        }
        ensure!(
            destination
                .metadata(&stage)
                .await?
                .is_some_and(|m| m.kind == Kind::Directory),
            "暂存路径不是目录"
        );
        let identity = self.id.to_string();
        let exists = destination.metadata(&marker).await?.is_some();
        let mut bytes = [0; 64];
        let n = if exists {
            let mut f = destination.open(&marker, false).await?;
            let n = f.read(0, &mut bytes).await?;
            f.close().await?;
            n
        } else {
            0
        };
        if bytes[..n] != *identity.as_bytes() {
            // Creation can be interrupted before the marker is flushed. Only reclaim an
            // empty stage with a missing marker or an exact prefix of this job's identity.
            ensure!(
                identity.as_bytes().starts_with(&bytes[..n]),
                "暂存目录不属于此传输，拒绝继续"
            );
            let mut directory = destination.directory(&stage).await?;
            while let Some(name) = directory.next().await? {
                ensure!(name == "owner", "暂存目录身份不完整且包含数据，拒绝清理");
            }
            directory.close().await?;
            let mut f = if exists {
                destination.resume(&marker).await?
            } else {
                destination.open(&marker, true).await?
            };
            f.write(0, identity.as_bytes()).await?;
            f.flush().await?;
            f.close().await?;
        }
        let mut f = destination.open(&marker, false).await?;
        let mut bytes = [0; 64];
        let n = f.read(0, &mut bytes).await?;
        f.close().await?;
        ensure!(
            bytes[..n] == *identity.as_bytes(),
            "暂存目录不属于此传输，拒绝继续"
        );
        Ok(())
    }
    async fn clean_empty_stage(&self, destination: &Endpoint) -> Result<()> {
        let marker = endpoint::join(&self.stage(), "owner");
        if destination.metadata(&marker).await?.is_some() {
            destination.remove(&marker, false).await?;
        }
        if destination.metadata(&self.stage()).await?.is_some() {
            destination.remove(&self.stage(), true).await?;
        }
        Ok(())
    }
    async fn copy_file(
        &self,
        source: &Endpoint,
        destination: &Endpoint,
        paths: CopyPaths<'_>,
        metadata: &Metadata,
        progress: &tokio::sync::watch::Sender<Progress>,
    ) -> Result<()> {
        let CopyPaths {
            source: src,
            target: dst,
            partial,
        } = paths;
        if destination.metadata(dst).await?.is_some() {
            verify_file(source, destination, src, dst, metadata.size).await?;
            return Ok(());
        }
        let mut input = source.open(src, false).await?;
        let mut output = if destination.metadata(partial).await?.is_some() {
            destination.resume(partial).await?
        } else {
            destination.open(partial, true).await?
        };
        let offset = destination
            .metadata(partial)
            .await?
            .context("分片丢失")?
            .size;
        ensure!(offset <= metadata.size, "分片大小与源文件不匹配");
        if offset > 0 {
            self.publish_progress(progress, self.bytes, State::Verifying);
            verify_prefix(&mut input, &mut output, offset).await?;
        }
        let mut buffer = vec![0; CHUNK];
        let mut copied = offset;
        let started = tokio::time::Instant::now();
        let chunk = if self.request.limit == 0 {
            CHUNK
        } else {
            (self.request.limit / 10).clamp(1, CHUNK as u64) as usize
        };
        while copied < metadata.size {
            let wanted = (metadata.size - copied).min(chunk as u64) as usize;
            read_exact(&mut input, copied, &mut buffer[..wanted]).await?;
            if self.request.limit > 0 {
                tokio::time::sleep_until(
                    started
                        + Duration::from_secs_f64(
                            (copied - offset + wanted as u64) as f64 / self.request.limit as f64,
                        ),
                )
                .await;
            }
            output.write(copied, &buffer[..wanted]).await?;
            copied += wanted as u64;
            self.publish_progress(progress, self.bytes + copied, State::Running);
        }
        ensure!(
            source
                .metadata(src)
                .await?
                .is_some_and(|m| metadata.unchanged(&m)),
            "传输期间源文件变化，未发布目标"
        );
        output.flush().await?;
        output.close().await?;
        input.close().await?;
        destination.permissions(partial, metadata).await?;
        destination.rename(partial, dst).await?;
        Ok(())
    }
    async fn reconcile(
        &mut self,
        root: &Path,
        source: &Endpoint,
        destination: &Endpoint,
        progress: &tokio::sync::watch::Sender<Progress>,
    ) -> Result<()> {
        if destination
            .metadata(&endpoint::join(&self.stage(), "data"))
            .await?
            .is_some()
        {
            self.ensure_stage(destination).await?;
            tokio::time::timeout(Duration::from_secs(30), self.publish(source, destination))
                .await
                .context("文件发布超时，可重试核对结果")??;
        } else {
            self.publish_progress(progress, self.bytes, State::Verifying);
            let mut plan = tokio::io::BufReader::new(
                tokio::fs::File::open(self.folder(root).join("plan.jsonl")).await?,
            );
            while let Some((e, _)) = read_entry(&mut plan).await? {
                let src = endpoint::join(self.source(), &e.relative);
                let dst = endpoint::join(self.target(), &e.relative);
                let found = destination
                    .metadata(&dst)
                    .await?
                    .context("无法确认发布结果，目标不存在")?;
                ensure!(found.kind == e.metadata.kind, "发布结果类型不匹配");
                match e.metadata.kind {
                    Kind::File => {
                        verify_file(source, destination, &src, &dst, e.metadata.size).await?
                    }
                    Kind::Symlink => ensure!(
                        destination.read_link(&dst).await? == e.link.context("链接目标丢失")?,
                        "链接发布结果不匹配"
                    ),
                    Kind::Directory => {}
                }
            }
        }
        self.state = State::Complete;
        self.save(root).await?;
        self.clean_empty_stage(destination).await?;
        Ok(())
    }
    pub(super) async fn discard(&self, destination: &Endpoint) -> Result<()> {
        if self.target.is_none() || destination.metadata(&self.stage()).await?.is_none() {
            return Ok(());
        }
        self.ensure_stage(destination).await?;
        // Keep the ownership marker until every data entry has been removed, so a
        // second cancellation can resume cleanup without losing the task identity.
        let stage = self.stage();
        let mut directory = destination.directory(&stage).await?;
        while let Some(name) = directory.next().await? {
            if name != "owner" {
                endpoint::valid_name(&name)?;
                remove_tree(destination, &endpoint::join(&stage, &name), 1).await?;
            }
        }
        directory.close().await?;
        self.clean_empty_stage(destination).await
    }
}
async fn resolve(endpoint: &Endpoint, path: &str) -> Result<String> {
    if matches!(endpoint, Endpoint::Local) {
        let path = Path::new(path);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("缺少文件名称")?;
        endpoint::valid_name(name)?;
        let parent = path
            .parent()
            .and_then(|p| p.to_str())
            .context("缺少父目录")?;
        return Ok(endpoint::join(&endpoint.canonical(parent).await?, name));
    }
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = trimmed.rsplit_once('/').unwrap_or((".", trimmed));
    endpoint::valid_name(name)?;
    let parent = endpoint
        .canonical(if parent.is_empty() { "/" } else { parent })
        .await?;
    Ok(endpoint::join(&parent, name))
}
async fn read_exact(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<()> {
    let mut pos = 0;
    while pos < buffer.len() {
        let n = file.read(offset + pos as u64, &mut buffer[pos..]).await?;
        ensure!(n > 0, "源文件或分片意外结束");
        pos += n;
    }
    Ok(())
}
async fn verify_prefix(source: &mut File, target: &mut File, length: u64) -> Result<()> {
    let mut a = vec![0; CHUNK];
    let mut b = vec![0; CHUNK];
    let mut offset = 0;
    while offset < length {
        let n = (length - offset).min(CHUNK as u64) as usize;
        read_exact(source, offset, &mut a[..n]).await?;
        read_exact(target, offset, &mut b[..n]).await?;
        ensure!(a[..n] == b[..n], "源文件或分片已变化，拒绝拼接不同内容");
        offset += n as u64;
    }
    Ok(())
}
pub(super) async fn verify_file(
    source: &Endpoint,
    target: &Endpoint,
    src: &str,
    dst: &str,
    length: u64,
) -> Result<()> {
    ensure!(
        source
            .metadata(src)
            .await?
            .is_some_and(|m| m.kind == Kind::File && m.size == length)
            && target
                .metadata(dst)
                .await?
                .is_some_and(|m| m.kind == Kind::File && m.size == length),
        "文件大小不匹配"
    );
    let mut input = source.open(src, false).await?;
    let mut output = target.open(dst, false).await?;
    verify_prefix(&mut input, &mut output, length).await?;
    input.close().await?;
    output.close().await
}
async fn read_entry(
    reader: &mut tokio::io::BufReader<tokio::fs::File>,
) -> Result<Option<(Entry, u64)>> {
    let mut bytes = Vec::new();
    let count = reader
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(
        count <= MAX_LINE && bytes.last() == Some(&b'\n'),
        "传输清单行无效或过长"
    );
    let entry: Entry = serde_json::from_slice(&bytes)?;
    entry.validate()?;
    Ok(Some((entry, count as u64)))
}
#[expect(
    clippy::too_many_arguments,
    reason = "Recursive traversal borrows bounded counters and the output stream without shared state"
)]
fn build_plan<'a>(
    source: &'a Endpoint,
    root: &'a str,
    relative: String,
    plan: &'a mut tokio::fs::File,
    entries: &'a mut u64,
    bytes: &'a mut u64,
    total: &'a mut u64,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        ensure!(depth <= 64, "目录层级超过 64 层");
        endpoint::relative(&relative)?;
        let path = endpoint::join(root, &relative);
        let metadata = source.metadata(&path).await?.context("源文件不存在")?;
        let link = if metadata.kind == Kind::Symlink {
            Some(source.read_link(&path).await?)
        } else {
            None
        };
        if metadata.kind == Kind::File {
            *total = total.checked_add(metadata.size).context("传输大小溢出")?;
        }
        let mut entry = Entry {
            relative: relative.clone(),
            metadata,
            finish: false,
            link,
        };
        write_entry(plan, &entry, entries, bytes).await?;
        if entry.metadata.kind == Kind::Directory {
            let mut dir: Directory = source.directory(&path).await?;
            while let Some(name) = dir.next().await? {
                endpoint::valid_name(&name)?;
                let child = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                build_plan(source, root, child, plan, entries, bytes, total, depth + 1).await?;
            }
            dir.close().await?;
            entry.finish = true;
            write_entry(plan, &entry, entries, bytes).await?;
        }
        Ok(())
    })
}
async fn write_entry(
    plan: &mut tokio::fs::File,
    entry: &Entry,
    entries: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    entry.validate()?;
    let mut data = serde_json::to_vec(entry)?;
    data.push(b'\n');
    ensure!(data.len() <= MAX_LINE, "路径数据过长");
    *entries += 1;
    *bytes += data.len() as u64;
    ensure!(
        *entries <= MAX_ENTRIES && *bytes <= MAX_PLAN,
        "目录清单超过 100000 项或 64 MiB，请分批传输"
    );
    plan.write_all(&data).await?;
    Ok(())
}
fn remove_tree<'a>(
    endpoint: &'a Endpoint,
    path: &'a str,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        ensure!(depth <= 66, "暂存目录层级超限");
        let Some(meta) = endpoint.metadata(path).await? else {
            return Ok(());
        };
        if meta.kind == Kind::Directory {
            let mut writable = meta.clone();
            writable.mode = Some(0o700);
            endpoint.permissions(path, &writable).await?;
            let mut dir = endpoint.directory(path).await?;
            while let Some(name) = dir.next().await? {
                endpoint::valid_name(&name)?;
                remove_tree(endpoint, &endpoint::join(path, &name), depth + 1).await?;
            }
            dir.close().await?;
            endpoint.remove(path, true).await?;
        } else {
            endpoint.remove(path, false).await?;
        }
        Ok(())
    })
}
