//! Streaming filesystem operations; symbolic links are never traversed.
use anyhow::{Context as _, Result, bail, ensure};
use futures_util::{
    FutureExt as _, StreamExt as _,
    stream::{FuturesOrdered, FuturesUnordered},
};
use russh_sftp::{
    client::{RawSftpSession, error::Error},
    protocol::{FileAttributes, OpenFlags, Packet, StatusCode},
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::path::Path;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

/// A protocol-sized buffer bounds each transfer independently of file size.
pub(super) const CHUNK: usize = 32 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
    Symlink,
}
/// Windows records a directory bit on the symlink itself. Unix and SFTP do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinkKind {
    File,
    Directory,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Metadata {
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<u64>,
    pub mode: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Fingerprint {
    pub metadata: Metadata,
    pub sha256: Option<[u8; 32]>,
}
impl Metadata {
    pub(super) fn remote(a: FileAttributes) -> Result<Self> {
        let kind = if a.is_dir() {
            Kind::Directory
        } else if a.is_regular() {
            Kind::File
        } else if a.is_symlink() {
            Kind::Symlink
        } else {
            bail!("不支持传输设备、管道或未知文件类型");
        };
        ensure!(
            kind != Kind::File || a.size.is_some(),
            "服务器未报告文件大小，拒绝传输"
        );
        Ok(Self {
            kind,
            size: a.size.unwrap_or(0),
            modified: a.mtime.map(u64::from),
            mode: a.permissions.map(|p| p & 0o777),
        })
    }
    fn local(a: std::fs::Metadata) -> Result<Self> {
        let kind = if a.is_dir() {
            Kind::Directory
        } else if a.is_file() {
            Kind::File
        } else if a.is_symlink() {
            Kind::Symlink
        } else {
            bail!("不支持传输设备、管道或未知文件类型");
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            Some(a.permissions().mode() & 0o777)
        };
        #[cfg(not(unix))]
        let mode = None;
        Ok(Self {
            kind,
            size: a.len(),
            modified: a
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|t| t.as_secs()),
            mode,
        })
    }
    pub fn unchanged(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.modified == other.modified
            && (self.kind != Kind::File || self.size == other.size)
    }
}
#[derive(Clone)]
pub(super) enum Endpoint {
    Local,
    Remote(Arc<RawSftpSession>),
}
pub(super) enum File {
    Local(tokio::fs::File),
    Remote {
        session: Arc<RawSftpSession>,
        handle: String,
    },
}
pub(super) enum Directory {
    Local(Box<tokio::fs::ReadDir>),
    Remote {
        session: Arc<RawSftpSession>,
        handle: String,
        page: std::collections::VecDeque<String>,
        empty: u8,
    },
}
impl Drop for File {
    fn drop(&mut self) {
        if let Self::Remote { session, handle } = self
            && !handle.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let s = session.clone();
            let h = std::mem::take(handle);
            runtime.spawn(async move {
                let _ = s.close(h).await;
            });
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if let Self::Remote {
            session, handle, ..
        } = self
            && !handle.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let s = session.clone();
            let h = std::mem::take(handle);
            runtime.spawn(async move {
                let _ = s.close(h).await;
            });
        }
    }
}
impl File {
    pub async fn close(mut self) -> Result<()> {
        if let Self::Remote { session, handle } = &mut self {
            session.close(std::mem::take(handle)).await?;
        }
        Ok(())
    }
    pub async fn read(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        match self {
            Self::Local(file) => {
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                Ok(file.read(buffer).await?)
            }
            Self::Remote { session, handle } => match session
                .read(&*handle, offset, buffer.len().min(CHUNK) as u32)
                .await
            {
                Ok(data) => {
                    ensure!(
                        !data.data.is_empty() && data.data.len() <= buffer.len(),
                        "无效的 SFTP 数据块"
                    );
                    buffer[..data.data.len()].copy_from_slice(&data.data);
                    Ok(data.data.len())
                }
                Err(Error::Status(status)) if status.status_code == StatusCode::Eof => Ok(0),
                Err(e) => Err(e.into()),
            },
        }
    }
    pub async fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Local(file) => {
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                file.write_all(bytes).await?;
            }
            Self::Remote { session, handle } => {
                session.write(&*handle, offset, bytes.to_vec()).await?;
            }
        }
        Ok(())
    }
    pub async fn flush(&mut self) -> Result<()> {
        match self {
            Self::Local(f) => f.sync_all().await?,
            Self::Remote { session, handle } => match session.fsync(&*handle).await {
                Ok(_) => {}
                Err(Error::Status(s)) if s.status_code == StatusCode::OpUnsupported => {}
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }
}
/// SFTP requests kept in flight per remote file, so a sequential transfer is not
/// limited to one chunk per round trip. Bounds buffered data to `PIPELINE * CHUNK`.
const PIPELINE: usize = 16;
type Pending<T> = Pin<Box<dyn Future<Output = Result<(T, Duration)>> + Send>>;
/// How many requests may be in flight. Each SFTP request's timeout runs from when it
/// is sent, and on a bandwidth-bound link a request waits behind every chunk sent
/// before it. So the window starts at one request, as before pipelining, grows while
/// replies return quickly (high latency, ample bandwidth) and shrinks when they slow
/// down, keeping each request far inside the 30 s request timeout.
struct Window(usize);
impl Window {
    const GROW_BELOW: Duration = Duration::from_secs(5);
    const SHRINK_ABOVE: Duration = Duration::from_secs(10);
    fn new() -> Self {
        Self(1)
    }
    fn get(&self) -> usize {
        self.0
    }
    fn observe(&mut self, elapsed: Duration) {
        if elapsed < Self::GROW_BELOW {
            self.0 = (self.0 * 2).min(PIPELINE);
        } else if elapsed > Self::SHRINK_ABOVE {
            self.0 = (self.0 / 2).max(1);
        }
    }
}
/// Runs `request`, timing it from its first poll, which is when it is sent.
fn timed<T: Send + 'static>(
    request: impl Future<Output = Result<T>> + Send + 'static,
) -> Pending<T> {
    Box::pin(async move {
        let sent = tokio::time::Instant::now();
        let value = request.await?;
        Ok((value, sent.elapsed()))
    })
}
async fn read_at(
    session: Arc<RawSftpSession>,
    handle: String,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>> {
    match session.read(handle, offset, len as u32).await {
        Ok(data) => {
            ensure!(
                !data.data.is_empty() && data.data.len() <= len,
                "无效的 SFTP 数据块"
            );
            Ok(data.data)
        }
        Err(Error::Status(status)) if status.status_code == StatusCode::Eof => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}
/// Reads a byte range in order as chunks of a fixed size, prefetching remote chunks.
pub(super) struct Reader<'a> {
    file: &'a mut File,
    position: u64,
    requested: u64,
    end: u64,
    chunk: usize,
    ended: &'static str,
    pending: FuturesOrdered<Pending<(u64, usize, Vec<u8>)>>,
    window: Window,
}
impl<'a> Reader<'a> {
    pub fn new(file: &'a mut File, start: u64, end: u64, chunk: usize) -> Self {
        Self {
            file,
            position: start,
            requested: start,
            end,
            chunk: chunk.clamp(1, CHUNK),
            ended: "源文件或分片意外结束",
            pending: FuturesOrdered::new(),
            window: Window::new(),
        }
    }
    /// The error reported when the file is shorter than the range.
    pub fn ended(mut self, message: &'static str) -> Self {
        self.ended = message;
        self
    }
    /// The next `min(chunk, remaining)` bytes, or `None` at the end of the range.
    /// A file shorter than the range is an error.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        if self.position >= self.end {
            return Ok(None);
        }
        let wanted = (self.end - self.position).min(self.chunk as u64) as usize;
        let File::Remote { session, handle } = &*self.file else {
            let mut data = vec![0; wanted];
            let mut filled = 0;
            while filled < wanted {
                let n = self
                    .file
                    .read(self.position + filled as u64, &mut data[filled..])
                    .await?;
                ensure!(n > 0, "{}", self.ended);
                filled += n;
            }
            self.position += wanted as u64;
            return Ok(Some(data));
        };
        while self.pending.len() < self.window.get() && self.requested < self.end {
            let len = (self.end - self.requested).min(self.chunk as u64) as usize;
            let (session, handle, offset) = (session.clone(), handle.clone(), self.requested);
            self.pending.push_back(timed(async move {
                Ok((offset, len, read_at(session, handle, offset, len).await?))
            }));
            self.requested += len as u64;
        }
        let ((offset, len, mut data), elapsed) =
            self.pending.next().await.context(self.ended)??;
        self.window.observe(elapsed);
        // A short reply is legal; fill the gap before any prefetched chunk is used.
        while data.len() < len {
            let more = read_at(
                session.clone(),
                handle.clone(),
                offset + data.len() as u64,
                len - data.len(),
            )
            .await?;
            ensure!(!more.is_empty(), "{}", self.ended);
            data.extend_from_slice(&more);
        }
        self.position += len as u64;
        Ok(Some(data))
    }
}
/// Writes chunks at explicit offsets, keeping remote writes in flight until `finish`.
pub(super) struct Writer<'a> {
    file: &'a mut File,
    pending: FuturesUnordered<Pending<()>>,
    window: Window,
}
impl<'a> Writer<'a> {
    pub fn new(file: &'a mut File) -> Self {
        Self {
            file,
            pending: FuturesUnordered::new(),
            window: Window::new(),
        }
    }
    pub async fn write(&mut self, offset: u64, bytes: Vec<u8>) -> Result<()> {
        let File::Remote { session, handle } = &*self.file else {
            return self.file.write(offset, &bytes).await;
        };
        let (session, handle) = (session.clone(), handle.clone());
        self.pending.push(timed(async move {
            session.write(handle, offset, bytes).await?;
            Ok(())
        }));
        // Polling once sends the request now and collects acknowledgements that arrived.
        while let Some(Some(result)) = self.pending.next().now_or_never() {
            self.window.observe(result?.1);
        }
        while self.pending.len() >= self.window.get() {
            if let Some(result) = self.pending.next().await {
                self.window.observe(result?.1);
            }
        }
        Ok(())
    }
    /// Waits for every acknowledgement; only then is the written range durable to flush.
    pub async fn finish(mut self) -> Result<()> {
        while let Some(result) = self.pending.next().await {
            result?;
        }
        Ok(())
    }
}
impl Directory {
    pub async fn close(mut self) -> Result<()> {
        if let Self::Remote {
            session, handle, ..
        } = &mut self
        {
            session.close(std::mem::take(handle)).await?;
        }
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Option<String>> {
        match self {
            Self::Local(dir) => match dir.next_entry().await? {
                Some(entry) => Ok(Some(
                    entry
                        .file_name()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("文件名不是 UTF-8"))?,
                )),
                None => Ok(None),
            },
            Self::Remote {
                session,
                handle,
                page,
                empty,
            } => loop {
                if let Some(name) = page.pop_front() {
                    if name == "." || name == ".." {
                        continue;
                    }
                    valid_name(&name)?;
                    return Ok(Some(name));
                }
                match session.readdir(&*handle).await {
                    Ok(reply) => {
                        ensure!(
                            reply.files.len() <= 1024
                                && reply.files.iter().map(|e| e.filename.len()).sum::<usize>()
                                    <= 64 * 1024,
                            "SFTP 目录分页超过内存限制"
                        );
                        if reply.files.is_empty() {
                            *empty += 1;
                            ensure!(*empty <= 4, "SFTP 连续返回空目录页");
                        } else {
                            *empty = 0;
                        }
                        *page = reply.files.into_iter().map(|e| e.filename).collect();
                    }
                    Err(Error::Status(s)) if s.status_code == StatusCode::Eof => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            },
        }
    }
}
impl Endpoint {
    pub async fn fingerprint(&self, path: &str) -> Result<Option<Fingerprint>> {
        use sha2::{Digest as _, Sha256};
        let Some(metadata) = self.metadata(path).await? else {
            return Ok(None);
        };
        let sha256 = if metadata.kind == Kind::File {
            let mut file = self.open(path, false).await?;
            let mut hasher = Sha256::new();
            let mut reader =
                Reader::new(&mut file, 0, metadata.size, CHUNK).ended("目标在校验期间被截断");
            while let Some(chunk) = reader.next().await? {
                hasher.update(&chunk);
            }
            file.close().await?;
            ensure!(
                self.metadata(path).await?.as_ref() == Some(&metadata),
                "目标在校验期间已变化"
            );
            Some(hasher.finalize().into())
        } else {
            None
        };
        Ok(Some(Fingerprint { metadata, sha256 }))
    }

    /// Atomically replaces a regular file, preserving its owner/group and permissions.
    /// Callers must compare the content fingerprint immediately before this commit.
    pub async fn replace_file(&self, source: &str, target: &str) -> Result<()> {
        match self {
            Self::Local => {
                let original = tokio::fs::symlink_metadata(target).await?;
                ensure!(original.is_file(), "只能替换普通文件");
                let source = source.to_owned();
                let target = target.to_owned();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt as _;
                        std::os::unix::fs::chown(
                            &source,
                            Some(original.uid()),
                            Some(original.gid()),
                        )
                        .context("无法保留目标文件所有者和组")?;
                    }
                    // FlushFileBuffers on Windows requires a handle opened for writing.
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&source)?
                        .sync_all()?;
                    std::fs::set_permissions(&source, original.permissions())?;
                    std::fs::rename(&source, &target)?;
                    #[cfg(unix)]
                    std::fs::File::open(Path::new(&target).parent().context("缺少目标目录")?)?
                        .sync_all()?;
                    Ok(())
                })
                .await??;
            }
            Self::Remote(session) => {
                let original = session.lstat(target).await?.attrs;
                ensure!(original.is_regular(), "只能替换普通文件");
                let staged = self.resume(source).await?;
                if let File::Remote { handle, .. } = &staged {
                    let attrs = session.fstat(handle).await?.attrs;
                    if attrs.uid != original.uid || attrs.gid != original.gid {
                        session
                            .fsetstat(
                                handle,
                                FileAttributes {
                                    uid: original.uid,
                                    gid: original.gid,
                                    ..Default::default()
                                },
                            )
                            .await
                            .context("无法保留目标文件所有者和组")?;
                    }
                    session
                        .fsetstat(
                            handle,
                            FileAttributes {
                                permissions: original.permissions,
                                ..Default::default()
                            },
                        )
                        .await?;
                    let attrs = session.fstat(handle).await?.attrs;
                    ensure!(
                        attrs.uid == original.uid
                            && attrs.gid == original.gid
                            && attrs.permissions == original.permissions,
                        "目标文件属性无法保留"
                    );
                }
                staged.close().await?;
                let mut request = Vec::new();
                for path in [source, target] {
                    request.extend_from_slice(&u32::try_from(path.len())?.to_be_bytes());
                    request.extend_from_slice(path.as_bytes());
                }
                match session
                    .extended("posix-rename@openssh.com", request)
                    .await?
                {
                    Packet::Status(status) if status.status_code == StatusCode::Ok => {}
                    _ => bail!("服务器不支持原子替换或拒绝操作，原文件保留，请选择保留两份"),
                }
            }
        }
        Ok(())
    }
    pub async fn metadata(&self, path: &str) -> Result<Option<Metadata>> {
        match self {
            Self::Local => match tokio::fs::symlink_metadata(path).await {
                Ok(m) => Ok(Some(Metadata::local(m)?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Self::Remote(s) => match s.lstat(path).await {
                Ok(m) => Ok(Some(Metadata::remote(m.attrs)?)),
                Err(Error::Status(e)) if e.status_code == StatusCode::NoSuchFile => Ok(None),
                Err(e) => Err(e.into()),
            },
        }
    }
    pub async fn canonical(&self, path: &str) -> Result<String> {
        match self {
            Self::Local => {
                let path = tokio::fs::canonicalize(path)
                    .await?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("路径不是 UTF-8"))?;
                // Preserve Windows verbatim/UNC prefixes. Forward slashes are not
                // interchangeable inside a verbatim path returned by canonicalize.
                Ok(path)
            }
            Self::Remote(s) => {
                let result = s.realpath(path).await?;
                ensure!(result.files.len() == 1, "SFTP 未返回唯一的绝对路径");
                let p = result
                    .files
                    .into_iter()
                    .next()
                    .context("缺少路径")?
                    .filename;
                ensure!(p.starts_with('/') && !p.contains('\0'), "SFTP 绝对路径无效");
                Ok(p)
            }
        }
    }
    pub async fn directory(&self, path: &str) -> Result<Directory> {
        ensure!(
            self.metadata(path)
                .await?
                .is_some_and(|m| m.kind == Kind::Directory),
            "目录不存在或是符号链接"
        );
        match self {
            Self::Local => Ok(Directory::Local(Box::new(tokio::fs::read_dir(path).await?))),
            Self::Remote(s) => Ok(Directory::Remote {
                session: s.clone(),
                handle: s.opendir(path).await?.handle,
                page: Default::default(),
                empty: 0,
            }),
        }
    }
    pub async fn mkdir(&self, path: &str) -> Result<()> {
        match self {
            Self::Local => {
                let b = tokio::fs::DirBuilder::new();
                #[cfg(unix)]
                let mut b = b;
                #[cfg(unix)]
                b.mode(0o700);
                b.create(path).await?;
            }
            Self::Remote(s) => {
                s.mkdir(
                    path,
                    FileAttributes {
                        permissions: Some(0o700),
                        ..Default::default()
                    },
                )
                .await?;
            }
        }
        Ok(())
    }
    pub async fn open(&self, path: &str, create: bool) -> Result<File> {
        if !create {
            ensure!(
                self.metadata(path)
                    .await?
                    .is_some_and(|m| m.kind == Kind::File),
                "文件不存在或不是普通文件"
            );
        }
        match self {
            Self::Local => {
                let mut o = tokio::fs::OpenOptions::new();
                o.read(true).write(create).create_new(create);
                #[cfg(unix)]
                {
                    o.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
                }
                Ok(File::Local(o.open(path).await?))
            }
            Self::Remote(s) => {
                let flags = if create {
                    OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE
                } else {
                    OpenFlags::READ
                };
                Ok(File::Remote {
                    session: s.clone(),
                    handle: s
                        .open(
                            path,
                            flags,
                            FileAttributes {
                                permissions: Some(0o600),
                                ..Default::default()
                            },
                        )
                        .await?
                        .handle,
                })
            }
        }
    }
    pub async fn resume(&self, path: &str) -> Result<File> {
        ensure!(
            self.metadata(path)
                .await?
                .is_some_and(|m| m.kind == Kind::File),
            "分片不是普通文件"
        );
        match self {
            Self::Local => {
                let mut o = tokio::fs::OpenOptions::new();
                o.read(true).write(true);
                #[cfg(unix)]
                o.custom_flags(nix::libc::O_NOFOLLOW);
                Ok(File::Local(o.open(path).await?))
            }
            Self::Remote(s) => Ok(File::Remote {
                session: s.clone(),
                handle: s
                    .open(
                        path,
                        OpenFlags::READ | OpenFlags::WRITE,
                        FileAttributes::default(),
                    )
                    .await?
                    .handle,
            }),
        }
    }
    pub async fn permissions(&self, path: &str, m: &Metadata) -> Result<()> {
        if m.kind == Kind::Symlink {
            return Ok(());
        }
        if let Some(mode) = m.mode {
            match self {
                Self::Local => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        let path = path.to_owned();
                        // Like cp and sftp, a peer's mode never widens what the umask
                        // allows locally, and never carries setuid, setgid or sticky bits.
                        tokio::task::spawn_blocking(move || {
                            std::fs::set_permissions(
                                path,
                                std::fs::Permissions::from_mode(mode & creation_mode()),
                            )
                        })
                        .await??;
                    }
                }
                Self::Remote(s) => {
                    s.setstat(
                        path,
                        FileAttributes {
                            permissions: Some(mode),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
    pub async fn read_link(&self, path: &str) -> Result<String> {
        match self {
            Self::Local => {
                let link = tokio::fs::read_link(path)
                    .await?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("链接目标不是 UTF-8"))?;
                // Windows returns the stored separator. Transfer records use '/'.
                #[cfg(windows)]
                let link = link.replace('\\', "/");
                Ok(link)
            }
            Self::Remote(s) => {
                let r = s.readlink(path).await?;
                ensure!(r.files.len() == 1, "无效链接目标");
                Ok(r.files.into_iter().next().context("缺少链接目标")?.filename)
            }
        }
    }
    pub async fn symlink(
        &self,
        path: &str,
        target: &str,
        #[cfg_attr(
            not(windows),
            expect(
                unused_variables,
                reason = "Unix and SFTP symlinks do not record a directory flag"
            )
        )]
        kind: LinkKind,
    ) -> Result<()> {
        match self {
            Self::Local => {
                #[cfg(unix)]
                tokio::fs::symlink(target, path).await?;
                #[cfg(windows)]
                {
                    let path = path.to_owned();
                    let target = target.to_owned();
                    tokio::task::spawn_blocking(move || {
                        create_windows_symlink(&target, &path, matches!(kind, LinkKind::Directory))
                    })
                    .await??;
                }
                #[cfg(not(any(unix, windows)))]
                bail!("此平台尚不支持恢复符号链接");
            }
            Self::Remote(s) => {
                s.symlink(path, target).await?;
            }
        }
        Ok(())
    }
    pub async fn rename(&self, source: &str, target: &str) -> Result<()> {
        ensure!(
            self.metadata(target).await?.is_none(),
            "目标已存在，传输不会覆盖文件"
        );
        match self {
            Self::Local => {
                let (a, b) = (source.to_owned(), target.to_owned());
                tokio::task::spawn_blocking(move || rename_exclusive(&a, &b)).await??;
            }
            Self::Remote(s) => {
                s.rename(source, target).await?;
            }
        }
        Ok(())
    }
    pub async fn remove(&self, path: &str, dir: bool) -> Result<()> {
        match self {
            Self::Local => {
                if dir {
                    tokio::fs::remove_dir(path).await?;
                } else {
                    tokio::fs::remove_file(path).await?;
                }
            }
            Self::Remote(s) => {
                if dir {
                    s.rmdir(path).await?;
                } else {
                    s.remove(path).await?;
                }
            }
        }
        Ok(())
    }
}

/// Permission bits the process umask leaves on a new file (`0o777 & !umask`).
///
/// umask(2) can only be read by setting it, which briefly changes it for every
/// thread in the process. Instead the OS applies it to a probe file created with
/// mode 0o777. The umask of a running app is treated as fixed after the first use;
/// if the probe fails, group and other write access are withheld.
#[cfg(unix)]
pub(super) fn creation_mode() -> u32 {
    static MODE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let path = std::env::temp_dir().join(format!(".nutty-umask-{}", uuid::Uuid::new_v4()));
        let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o777)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
        else {
            return 0o755;
        };
        let mode = file.metadata().map(|m| m.permissions().mode() & 0o777);
        drop(file);
        let _ = std::fs::remove_file(&path);
        mode.unwrap_or(0o755)
    })
}
pub(super) fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && name.len() <= 4096
            && !name.contains(['/', '\\', '\0'])
            && !name.chars().any(char::is_control),
        "文件名称无效或无法跨平台安全传输"
    );
    #[cfg(windows)]
    {
        // Windows ignores trailing spaces before the extension when matching devices.
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let numbered_device = (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem
                .chars()
                .nth(3)
                .is_some_and(|digit| ('1'..='9').contains(&digit) || "¹²³".contains(digit));
        ensure!(
            !name.contains([':', '*', '?', '"', '<', '>', '|'])
                && !name.ends_with(['.', ' '])
                && !matches!(
                    stem.as_str(),
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                )
                && !numbered_device,
            "文件名称无法在 Windows 上安全保存"
        );
    }
    Ok(())
}
pub(super) fn join(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        root.into()
    } else if cfg!(windows) && (root.starts_with(r"\\") || root.as_bytes().get(1) == Some(&b':')) {
        format!(
            "{}\\{}",
            root.trim_end_matches(['/', '\\']),
            relative.replace('/', "\\")
        )
    } else {
        format!("{}/{}", root.trim_end_matches('/'), relative)
    }
}
pub(super) fn relative(path: &str) -> Result<()> {
    ensure!(path.len() <= 4096 && !path.starts_with('/'), "相对路径无效");
    if !path.is_empty() {
        for part in path.split('/') {
            valid_name(part)?;
        }
    }
    Ok(())
}
#[cfg(windows)]
fn create_windows_symlink(target: &str, path: &str, directory: bool) -> Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    /// `ERROR_PRIVILEGE_NOT_HELD` from `CreateSymbolicLinkW`.
    const PRIVILEGE_NOT_HELD: i32 = 1314;
    let created = if directory {
        symlink_dir(target, path)
    } else {
        symlink_file(target, path)
    };
    created.map_err(|error| {
        if error.raw_os_error() == Some(PRIVILEGE_NOT_HELD) {
            anyhow::anyhow!(
                "Windows 未允许创建符号链接。请开启开发人员模式，或以管理员身份运行后重试"
            )
        } else {
            error.into()
        }
    })
}
fn rename_exclusive(source: &str, target: &str) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let source = std::ffi::CString::new(source)?;
        let target = std::ffi::CString::new(target)?;
        // SAFETY: C strings live for the call. RENAME_EXCL/NOREPLACE forbids replacing any target.
        let result = unsafe {
            #[cfg(target_os = "macos")]
            {
                nix::libc::renamex_np(source.as_ptr(), target.as_ptr(), nix::libc::RENAME_EXCL)
            }
            #[cfg(target_os = "linux")]
            {
                nix::libc::renameat2(
                    nix::libc::AT_FDCWD,
                    source.as_ptr(),
                    nix::libc::AT_FDCWD,
                    target.as_ptr(),
                    nix::libc::RENAME_NOREPLACE,
                )
            }
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let parent = Path::new(target.to_str()?).parent().context("缺少父目录")?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let wide = |path: &str| -> Result<Vec<u16>> {
            ensure!(!path.contains('\0'), "路径包含空字符");
            Ok(std::ffi::OsStr::new(&path.replace('/', "\\"))
                .encode_wide()
                .chain(Some(0))
                .collect())
        };
        let source = wide(source)?;
        let target = wide(target)?;
        // SAFETY: NUL-terminated UTF-16 buffers live through the call. WRITE_THROUGH
        // requests durability; omitting REPLACE_EXISTING rejects existing targets.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x8) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (source, target);
        bail!("此平台尚不支持原子不覆盖目录发布")
    }
}
