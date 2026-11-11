//! Bounded text editing with content checks and atomic OpenSSH SFTP replacement.

use anyhow::{Context as _, Result, bail, ensure};
use russh_sftp::{
    client::{RawSftpSession, error::Error},
    protocol::{FileAttributes, OpenFlags, Packet, StatusCode},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Keeps editable content and its undo state within a predictable memory budget.
pub const MAX_EDIT_BYTES: usize = 512 * 1024;

pub async fn save(
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    path: &str,
    original: &str,
    replacement: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure!(
        original.len() <= MAX_EDIT_BYTES && replacement.len() <= MAX_EDIT_BYTES,
        "编辑只支持 512 KiB 以内的文本文件"
    );
    ensure!(!cancel.is_cancelled(), "保存已取消");
    let sftp = RawSftpSession::new(stream);
    let version = sftp.init().await?;
    ensure!(
        version
            .extensions
            .get("posix-rename@openssh.com")
            .is_some_and(|version| version == "1"),
        "此服务器不支持原子替换，请另存文件后上传"
    );
    let attributes = sftp.lstat(path).await?.attrs;
    ensure!(attributes.is_regular(), "只支持编辑普通文件");
    ensure!(
        read(&sftp, path, cancel).await? == original.as_bytes(),
        "远程文件已更改，请重新打开后合并修改"
    );
    let temporary = format!("{path}.nutty-{}.part", Uuid::new_v4());
    let result = async {
        let handle = sftp.open(&temporary, OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE, FileAttributes { permissions: Some(0o600), ..Default::default() }).await?.handle;
        let writing = async {
            for (index, chunk) in replacement.as_bytes().chunks(32 * 1024).enumerate() {
                tokio::select! { biased; () = cancel.cancelled() => bail!("保存已取消"), result = sftp.write(&handle, (index * 32 * 1024) as u64, chunk.to_vec()) => { result?; } }
            }
            let current = sftp.fstat(&handle).await?.attrs;
            // Ownership changes may clear mode bits, so restore ownership before permissions.
            if current.uid != attributes.uid || current.gid != attributes.gid {
                sftp.fsetstat(&handle, FileAttributes { uid: attributes.uid, gid: attributes.gid, ..Default::default() }).await.context("无法保留文件所有者或组，原文件未替换")?;
            }
            sftp.fsetstat(&handle, FileAttributes { permissions: attributes.permissions, ..Default::default() }).await.context("无法保留文件权限，原文件未替换")?;
            let restored = sftp.fstat(&handle).await?.attrs;
            ensure!(restored.permissions.map(|mode| mode & 0o7777) == attributes.permissions.map(|mode| mode & 0o7777) && restored.uid == attributes.uid && restored.gid == attributes.gid, "文件属性校验失败，原文件未替换");
            if version.extensions.get("fsync@openssh.com").is_some_and(|version| version == "1") { sftp.fsync(&handle).await?; }
            Ok::<_, anyhow::Error>(())
        }.await;
        let closing = sftp.close(&handle).await;
        writing?; closing?;
        // SFTP has no compare-and-swap operation; this detects changes observed before publication.
        let current = sftp.lstat(path).await?.attrs;
        ensure!(current.is_regular() && current.permissions == attributes.permissions && current.uid == attributes.uid && current.gid == attributes.gid && read(&sftp, path, cancel).await? == original.as_bytes(), "远程文件已更改，请重新打开后合并修改");
        ensure!(!cancel.is_cancelled(), "保存已取消");
        let mut request = Vec::new();
        for value in [&temporary, path] {
            request.extend_from_slice(&(u32::try_from(value.len())?).to_be_bytes());
            request.extend_from_slice(value.as_bytes());
        }
        match sftp.extended("posix-rename@openssh.com", request).await? {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            packet => bail!("原子保存未成功：{packet:?}"),
        }
    }.await;
    if result.is_err() {
        let _ = sftp.remove(&temporary).await;
    }
    let _ = sftp.close_session();
    result.context("保存远程文件")
}

async fn read(sftp: &RawSftpSession, path: &str, cancel: &CancellationToken) -> Result<Vec<u8>> {
    let handle = sftp
        .open(path, OpenFlags::READ, FileAttributes::default())
        .await?
        .handle;
    let result = async {
        let mut bytes = Vec::new();
        loop {
            let result = tokio::select! { biased; () = cancel.cancelled() => bail!("保存已取消"), result = sftp.read(&handle, bytes.len() as u64, 32 * 1024) => result };
            match result {
                Ok(data) => {
                    ensure!(!data.data.is_empty(), "服务器返回了空的文件块");
                    ensure!(bytes.len() + data.data.len() <= MAX_EDIT_BYTES, "文件超过编辑大小限制");
                    bytes.extend_from_slice(&data.data);
                }
                Err(Error::Status(status)) if status.status_code == StatusCode::Eof => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(bytes)
    }.await;
    let _ = sftp.close(&handle).await;
    result
}
