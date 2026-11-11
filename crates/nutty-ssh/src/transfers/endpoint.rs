//! Streaming filesystem operations; symbolic links are never traversed.
use anyhow::{Context as _, Result, bail, ensure};
use russh_sftp::{
    client::{RawSftpSession, error::Error},
    protocol::{FileAttributes, OpenFlags, Packet, StatusCode},
};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

/// A protocol-sized buffer bounds each transfer independently of file size.
pub(super) const CHUNK: usize = 32 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Directory,
    Symlink,
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
    Local(tokio::fs::ReadDir),
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
            let mut buffer = vec![0; CHUNK];
            let mut hasher = Sha256::new();
            let mut offset = 0;
            while offset < metadata.size {
                let wanted = (metadata.size - offset).min(CHUNK as u64) as usize;
                let n = file.read(offset, &mut buffer[..wanted]).await?;
                ensure!(n > 0, "目标在校验期间被截断");
                hasher.update(&buffer[..n]);
                offset += n as u64;
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
                    std::fs::set_permissions(&source, original.permissions())?;
                    std::fs::File::open(&source)?.sync_all()?;
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
                #[cfg(windows)]
                let path = path.replace('\\', "/");
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
            Self::Local => Ok(Directory::Local(tokio::fs::read_dir(path).await?)),
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
                let mut b = tokio::fs::DirBuilder::new();
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
                        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                            .await?;
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
            Self::Local => tokio::fs::read_link(path)
                .await?
                .into_os_string()
                .into_string()
                .map_err(|_| anyhow::anyhow!("链接目标不是 UTF-8")),
            Self::Remote(s) => {
                let r = s.readlink(path).await?;
                ensure!(r.files.len() == 1, "无效链接目标");
                Ok(r.files.into_iter().next().context("缺少链接目标")?.filename)
            }
        }
    }
    pub async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        match self {
            Self::Local => {
                #[cfg(unix)]
                tokio::fs::symlink(target, path).await?;
                #[cfg(not(unix))]
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
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        ensure!(
            !name.contains([':', '*', '?', '"', '<', '>', '|'])
                && !name.ends_with(['.', ' '])
                && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "文件名称无法在 Windows 上安全保存"
        );
    }
    Ok(())
}
pub(super) fn join(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        root.into()
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
