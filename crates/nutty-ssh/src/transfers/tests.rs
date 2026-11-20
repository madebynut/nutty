use super::endpoint::{Endpoint, LinkKind};
use super::*;
use std::path::Path;

async fn fixture(root: &Path, source: &Path, target: &Path) -> Result<Job> {
    let j = Job {
        version: 1,
        conflict: None,
        resolution: Resolution::Ask,
        replacement: None,
        skip_requested: false,
        restart_requested: false,
        cleanup_requested: false,
        id: Uuid::new_v4(),
        request: Request {
            connection: Uuid::new_v4(),
            local: source.into(),
            remote: target.to_str().context("path")?.into(),
            direction: Direction::Upload,
            route: Vec::new(),
            proxies: Vec::new(),
            limit: 0,
        },
        state: State::Queued,
        error: None,
        bytes: 0,
        total: 0,
        completed: 0,
        entries: 0,
        cursor: 0,
        source: None,
        target: None,
        manifest_ready: false,
        publishing: false,
        attempt: 0,
    };
    tokio::fs::create_dir_all(j.folder(root)).await?;
    j.save(root).await?;
    Ok(j)
}
fn progress() -> watch::Sender<Progress> {
    watch::channel(Progress {
        bytes: 0,
        total: 0,
        completed: 0,
        entries: 0,
        state: State::Queued,
    })
    .0
}

#[tokio::test]
async fn conflicts_can_keep_both_skip_and_replace_without_losing_original() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target.txt");
    tokio::fs::write(&source, b"new content").await?;
    tokio::fs::write(&target, b"original").await?;
    let mut job = fixture(dir.path(), &source, &target).await?;
    assert!(
        job.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(job.summary().replaceable);
    job.resolution = Resolution::KeepBoth;
    job.conflict = None;
    job.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(tokio::fs::read(&target).await?, b"original");
    assert_eq!(
        tokio::fs::read(dir.path().join("target (2).txt")).await?,
        b"new content"
    );

    let mut job = fixture(dir.path(), &source, &target).await?;
    assert!(
        job.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    job.skip_requested = true;
    job.discard(&Endpoint::Local).await?;
    finish_cleanup(&mut job);
    assert_eq!(job.state, State::Skipped);
    assert_eq!(tokio::fs::read(&target).await?, b"original");

    let mut job = fixture(dir.path(), &source, &target).await?;
    assert!(
        job.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    job.resolution = Resolution::ReplaceFile;
    job.replacement = job.conflict.take();
    // Same-size changes must not bypass confirmation, even within one timestamp second.
    tokio::fs::write(&target, b"modified").await?;
    assert!(
        job.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(tokio::fs::read(&target).await?, b"modified");
    job.resolution = Resolution::ReplaceFile;
    job.replacement = job.conflict.take();
    job.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(tokio::fs::read(&target).await?, b"new content");
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn openssh_replace_preserves_mode_and_unsupported_extension_keeps_original() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Stdio;
    for supported in [true, false] {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        tokio::fs::write(&source, b"replacement").await?;
        tokio::fs::write(&target, b"original").await?;
        tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).await?;
        let mut command = tokio::process::Command::new("/usr/libexec/sftp-server");
        if !supported {
            command.args(["-P", "posix-rename"]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let raw = russh_sftp::client::RawSftpSession::new(tokio::io::join(
            child.stdout.take().context("stdout")?,
            child.stdin.take().context("stdin")?,
        ));
        raw.init().await?;
        let endpoint = Endpoint::Remote(Arc::new(raw));
        let mut job = fixture(dir.path(), &source, &target).await?;
        assert!(
            job.execute(
                dir.path(),
                &Endpoint::Local,
                &endpoint,
                &progress(),
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        job.resolution = Resolution::ReplaceFile;
        job.replacement = job.conflict.take();
        let result = job
            .execute(
                dir.path(),
                &Endpoint::Local,
                &endpoint,
                &progress(),
                &CancellationToken::new(),
            )
            .await;
        if supported {
            result?;
            assert_eq!(tokio::fs::read(&target).await?, b"replacement");
        } else {
            assert!(result.is_err());
            assert_eq!(tokio::fs::read(&target).await?, b"original");
        }
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o640
        );
        child.kill().await?;
    }
    Ok(())
}
#[tokio::test]
async fn recursive_transfer_is_atomic_bounded_and_preserves_links_and_modes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("source");
    let dest = dir.path().join("copy");
    tokio::fs::create_dir_all(src.join("中文/empty")).await?;
    tokio::fs::write(src.join("中文/file"), b"payload").await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(
            src.join("中文/file"),
            std::fs::Permissions::from_mode(0o640),
        )
        .await?;
        tokio::fs::symlink("中文/file", src.join("link")).await?;
        tokio::fs::symlink("中文", src.join("dir-link")).await?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        symlink_file("中文/file", src.join("link"))?;
        symlink_dir("中文", src.join("dir-link"))?;
    }
    let mut j = fixture(dir.path(), &src, &dest).await?;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(j.state, State::Complete);
    assert_eq!(tokio::fs::read(dest.join("中文/file")).await?, b"payload");
    assert!(dest.join("中文/empty").is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(dest.join("中文/file"))?
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
    #[cfg(any(unix, windows))]
    {
        assert_eq!(
            Endpoint::Local
                .read_link(dest.join("link").to_str().context("path")?)
                .await?,
            "中文/file"
        );
        assert_eq!(
            Endpoint::Local
                .read_link(dest.join("dir-link").to_str().context("path")?)
                .await?,
            "中文"
        );
    }
    assert!(!dir.path().join(format!(".nutty-{}.part", j.id)).exists());
    Ok(())
}
#[tokio::test]
async fn symlink_target_kind_follows_directories_files_and_dangling_links() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    tokio::fs::create_dir(root.join("real")).await?;
    tokio::fs::write(root.join("real/file"), b"x").await?;
    let root = root.to_str().context("path")?;
    let join = |name: &str| super::endpoint::join(root, name);
    let local = Endpoint::Local;
    local
        .symlink(&join("file-link"), "real/file", LinkKind::File)
        .await?;
    local
        .symlink(&join("dir-link"), "real", LinkKind::Directory)
        .await?;
    local
        .symlink(&join("dangling"), "missing", LinkKind::File)
        .await?;
    local
        .symlink(&join("via"), "dir-link", LinkKind::File)
        .await?;
    assert_eq!(
        job::link_target_kind(&local, &join("file-link"), "real/file").await?,
        LinkKind::File
    );
    assert_eq!(
        job::link_target_kind(&local, &join("dir-link"), "real").await?,
        LinkKind::Directory
    );
    assert_eq!(
        job::link_target_kind(&local, &join("dangling"), "missing").await?,
        LinkKind::File
    );
    assert_eq!(
        job::link_target_kind(&local, &join("via"), "dir-link").await?,
        LinkKind::Directory
    );
    assert_eq!(local.read_link(&join("dir-link")).await?, "real");
    Ok(())
}
#[tokio::test]
async fn reload_accepts_local_absolute_paths_and_rejects_relative_ones() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.bin");
    let target = dir.path().join("target.bin");
    tokio::fs::write(&source, b"x").await?;
    let mut job = fixture(dir.path(), &source, &target).await?;
    job.source = Some(source.to_str().context("path")?.into());
    job.target = Some(target.to_str().context("path")?.into());
    job.save(dir.path()).await?;
    let restored = job::read_job(&job.folder(dir.path()).join("task.json")).await?;
    assert_eq!(restored.source.as_deref(), job.source.as_deref());
    assert_eq!(restored.target.as_deref(), job.target.as_deref());

    job.target = Some("relative.bin".into());
    assert!(job.validate().is_err());
    job.target = Some("/remote/target.bin".into());
    assert!(job.validate().is_ok());
    job.source = Some("relative.bin".into());
    assert!(job.validate().is_err());
    Ok(())
}
#[tokio::test]
async fn pause_restart_resumes_verified_partial_without_clobber() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.bin");
    let target = dir.path().join("target.bin");
    let payload = vec![31; 256 * 1024];
    tokio::fs::write(&source, &payload).await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    j.request.limit = 128 * 1024;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let p = progress();
    let mut rx = p.subscribe();
    let pause = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            if rx.borrow().bytes >= 32 * 1024 {
                c.cancel();
                break;
            }
        }
    });
    j.execute(dir.path(), &Endpoint::Local, &Endpoint::Local, &p, &cancel)
        .await?;
    pause.await?;
    assert_eq!(j.state, State::Paused);
    assert!(!target.exists());
    let mut restored = job::read_job(&j.folder(dir.path()).join("task.json")).await?;
    restored.request.limit = 0;
    restored
        .execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(restored.state, State::Complete);
    assert_eq!(tokio::fs::read(&target).await?, payload);
    let mut conflict = fixture(dir.path(), &source, &target).await?;
    assert!(
        conflict
            .execute(
                dir.path(),
                &Endpoint::Local,
                &Endpoint::Local,
                &progress(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&target).await?, payload);
    Ok(())
}
#[tokio::test]
async fn changed_partial_is_rejected_and_cancel_removes_only_owned_stage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("dest");
    tokio::fs::write(&source, vec![42; 128 * 1024]).await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    j.request.limit = 64 * 1024;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let p = progress();
    let mut rx = p.subscribe();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            if rx.borrow().bytes > 0 {
                c.cancel();
                break;
            }
        }
    });
    j.execute(dir.path(), &Endpoint::Local, &Endpoint::Local, &p, &cancel)
        .await?;
    let stage = dir.path().join(format!(".nutty-{}.part", j.id));
    let part = stage.join("part-0");
    tokio::fs::write(&part, b"changed").await?;
    j.request.limit = 0;
    assert!(
        j.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(!target.exists());
    j.discard(&Endpoint::Local).await?;
    assert!(!stage.exists());
    assert!(source.exists());
    Ok(())
}
#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_openssh_recursive_upload_and_download_round_trip() -> Result<()> {
    use std::process::Stdio;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("local");
    let remote = dir.path().join("remote");
    let downloaded = dir.path().join("downloaded");
    tokio::fs::create_dir_all(source.join("中文/empty")).await?;
    tokio::fs::write(source.join("中文/data"), vec![17; 256 * 1024]).await?;
    let mut child = tokio::process::Command::new("/usr/libexec/sftp-server")
        .args(["-u", "0077"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let raw = russh_sftp::client::RawSftpSession::new(tokio::io::join(
        child.stdout.take().context("stdout")?,
        child.stdin.take().context("stdin")?,
    ));
    raw.init().await?;
    let remote_endpoint = Endpoint::Remote(Arc::new(raw));
    let mut upload = fixture(dir.path(), &source, &remote).await?;
    upload
        .execute(
            dir.path(),
            &Endpoint::Local,
            &remote_endpoint,
            &progress(),
            &CancellationToken::new(),
        )
        .await?;
    let mut download = fixture(dir.path(), &downloaded, &remote).await?;
    download.request.direction = Direction::Download;
    download
        .execute(
            dir.path(),
            &remote_endpoint,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        tokio::fs::read(downloaded.join("中文/data")).await?,
        vec![17; 256 * 1024]
    );
    assert!(downloaded.join("中文/empty").is_dir());
    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn ambiguous_publish_reconciles_content_without_overwriting() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    tokio::fs::write(&source, b"verified").await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    j.state = State::Failed;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(j.state, State::Complete);
    tokio::fs::write(&target, b"modified").await?;
    j.state = State::Failed;
    assert!(
        j.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(tokio::fs::read(&target).await?, b"modified");
    Ok(())
}
#[tokio::test]
async fn excessive_depth_is_rejected_before_any_destination_is_published() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    let mut nested = source.clone();
    for _ in 0..66 {
        nested = nested.join("d");
    }
    tokio::fs::create_dir_all(nested).await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    let error = j
        .execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("bounded recursion");
    assert!(error.to_string().contains("64"));
    assert!(!target.exists());
    Ok(())
}
#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_connection_loss_keeps_partial_and_resumes_on_a_new_session() -> Result<()> {
    use std::process::Stdio;
    async fn server() -> Result<(tokio::process::Child, Endpoint)> {
        let mut child = tokio::process::Command::new("/usr/libexec/sftp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let raw = russh_sftp::client::RawSftpSession::new_with_config(
            tokio::io::join(
                child.stdout.take().context("stdout")?,
                child.stdin.take().context("stdin")?,
            ),
            russh_sftp::client::Config {
                request_timeout_secs: 1,
                ..Default::default()
            },
        );
        raw.init().await?;
        Ok((child, Endpoint::Remote(Arc::new(raw))))
    }
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    let payload = vec![51; 512 * 1024];
    tokio::fs::write(&source, &payload).await?;
    let (mut child, remote) = server().await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    j.request.limit = 256 * 1024;
    let p = progress();
    let mut rx = p.subscribe();
    let killed = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            if rx.borrow().bytes > 0 {
                child.kill().await?;
                return Ok::<_, anyhow::Error>(());
            }
        }
        Ok(())
    });
    assert!(
        j.execute(
            dir.path(),
            &Endpoint::Local,
            &remote,
            &p,
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    killed.await??;
    assert!(!target.exists());
    let (mut child, new_remote) = server().await?;
    j.request.limit = 0;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &new_remote,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(tokio::fs::read(&target).await?, payload);
    let _ = child.kill().await;
    Ok(())
}

#[tokio::test]
async fn restart_rescans_changed_source_and_reclaims_interrupted_marker() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    tokio::fs::write(&source, vec![1; 256 * 1024]).await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    j.request.limit = 64 * 1024;
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let p = progress();
    let mut rx = p.subscribe();
    let observer = tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            if rx.borrow().bytes > 0 {
                c.cancel();
                break;
            }
        }
    });
    j.execute(dir.path(), &Endpoint::Local, &Endpoint::Local, &p, &cancel)
        .await?;
    observer.await?;
    assert_eq!(j.state, State::Paused);
    tokio::fs::write(&source, b"new source version").await?;
    j.restart_requested = true;
    j.cleanup_requested = true;
    j.discard(&Endpoint::Local).await?;
    finish_cleanup(&mut j);
    assert!(!j.cleanup_requested);
    j.request.limit = 0;
    // Simulate interruption between mkdir and writing the ownership marker.
    let stage = dir.path().join(format!(".nutty-{}.part", j.id));
    tokio::fs::create_dir(&stage).await?;
    tokio::fs::write(stage.join("owner"), &j.id.to_string()[..8]).await?;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(j.state, State::Complete);
    assert_eq!(tokio::fs::read(&target).await?, b"new source version");
    assert!(!stage.exists());
    Ok(())
}

#[tokio::test]
async fn failed_resolution_can_retry_and_self_scanning_is_rejected() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("missing/target");
    tokio::fs::write(&source, b"content").await?;
    let mut j = fixture(dir.path(), &source, &target).await?;
    assert!(
        j.execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    j.validate()?;
    tokio::fs::create_dir(dir.path().join("missing")).await?;
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(tokio::fs::read(&target).await?, b"content");
    let mut recursive = fixture(dir.path(), dir.path(), &dir.path().join("copy")).await?;
    assert!(
        recursive
            .execute(
                dir.path(),
                &Endpoint::Local,
                &Endpoint::Local,
                &progress(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(!dir.path().join("copy").exists());
    Ok(())
}

/// Run explicitly with /usr/bin/time -l to measure the worker separately from the GUI.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "native SFTP resource probe; creates a 256 MiB file and 10000 directory entries"]
async fn native_sftp_resource_probe() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    tokio::fs::create_dir(&source).await?;
    let large = source.join("large");
    let mut f = tokio::fs::File::create(&large).await?;
    let buffer = vec![0x5a; endpoint::CHUNK];
    for _ in 0..8192 {
        f.write_all(&buffer).await?;
    }
    f.sync_all().await?;
    drop(f);
    for i in 0..10_000 {
        tokio::fs::write(source.join(format!("item-{i:05}")), b"small").await?;
    }
    let mut child = tokio::process::Command::new("/usr/libexec/sftp-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let raw = russh_sftp::client::RawSftpSession::new_with_config(
        tokio::io::join(
            child.stdout.take().context("stdout")?,
            child.stdin.take().context("stdin")?,
        ),
        russh_sftp::client::Config {
            max_packet_len: 128 * 1024,
            max_concurrent_reads: 1,
            max_concurrent_writes: 1,
            ..Default::default()
        },
    );
    raw.init().await?;
    let remote = Endpoint::Remote(Arc::new(raw));
    let mut j = fixture(dir.path(), &source, &target).await?;
    let start = std::time::Instant::now();
    j.execute(
        dir.path(),
        &Endpoint::Local,
        &remote,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    super::job::verify_file(
        &Endpoint::Local,
        &remote,
        large.to_str().context("path")?,
        target.join("large").to_str().context("path")?,
        256 * 1024 * 1024,
    )
    .await?;
    assert_eq!(j.state, State::Complete);
    assert_eq!(std::fs::read_dir(&target)?.count(), 10001);
    println!(
        "native SFTP probe: {} bytes, {} manifest entries, {:?}; fixed {} byte buffer",
        j.total,
        j.entries,
        start.elapsed(),
        endpoint::CHUNK
    );
    let _ = child.kill().await;
    Ok(())
}

#[test]
fn malformed_file_metadata_is_not_treated_as_an_empty_file() {
    let missing_size = russh_sftp::protocol::FileAttributes {
        permissions: Some(0o100644),
        ..Default::default()
    };
    assert!(endpoint::Metadata::remote(missing_size).is_err());
}

#[cfg(windows)]
#[test]
fn windows_device_names_are_rejected_for_downloads() {
    for name in [
        "CON", "con.txt", "CON .txt", "CONIN$", "CONOUT$", "COM1", "LPT9.log", "COM¹", "LPT³",
    ] {
        assert!(super::endpoint::valid_name(name).is_err(), "{name:?}");
    }
    for name in ["COM10", "CONSOLE.txt", "readme.md"] {
        assert!(super::endpoint::valid_name(name).is_ok(), "{name:?}");
    }
}
/// A remote endpoint on the in-memory SFTP fixture. Each reply reaches the client
/// `latency` after the server sends it, while later requests keep flowing, like a
/// network round trip.
async fn fixture_remote(files: &crate::sftp_test_server::Files, latency: Duration) -> Endpoint {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (client, relay_client) = tokio::io::duplex(1 << 20);
    let (relay_server, server) = tokio::io::duplex(1 << 20);
    russh_sftp::server::run(server, files.handler()).await;
    let (mut from_client, mut to_client) = tokio::io::split(relay_client);
    let (mut from_server, mut to_server) = tokio::io::split(relay_server);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut from_client, &mut to_server).await;
    });
    let (delayed, mut arriving) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
    tokio::spawn(async move {
        let mut buffer = vec![0; 64 * 1024];
        while let Ok(n @ 1..) = from_server.read(&mut buffer).await {
            let due = tokio::time::Instant::now() + latency;
            if delayed.send((due, buffer[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        while let Some((due, bytes)) = arriving.recv().await {
            tokio::time::sleep_until(due).await;
            if to_client.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    Endpoint::Remote(Arc::new(russh_sftp::client::RawSftpSession::new(client)))
}
async fn initialized(endpoint: Endpoint) -> Result<Endpoint> {
    if let Endpoint::Remote(session) = &endpoint {
        session.init().await?;
    }
    Ok(endpoint)
}
#[tokio::test(start_paused = true)]
async fn replacing_a_large_remote_target_hashes_it_outside_the_commit_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let files = crate::sftp_test_server::Files::seeded();
    // 128 sequential 32 KiB reads of the 4 MiB target take 38 s, beyond the 30 s commit.
    files.slow_reads(Duration::from_millis(300));
    let remote = initialized(fixture_remote(&files, Duration::ZERO).await).await?;
    let source = dir.path().join("replacement");
    tokio::fs::write(&source, b"replacement").await?;
    let mut job = fixture(dir.path(), &source, Path::new("/large.bin")).await?;
    let (progress, cancel) = (progress(), CancellationToken::new());
    assert!(
        job.execute(dir.path(), &Endpoint::Local, &remote, &progress, &cancel)
            .await
            .is_err(),
        "the existing file is a conflict"
    );
    job.resolution = Resolution::ReplaceFile;
    job.replacement = job.conflict.take();
    job.execute(dir.path(), &Endpoint::Local, &remote, &progress, &cancel)
        .await?;
    assert_eq!(job.state, State::Complete);
    assert_eq!(
        files.snapshot().await.get("large.bin").map(Vec::as_slice),
        Some(&b"replacement"[..])
    );
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn remote_transfers_keep_several_requests_in_flight() -> Result<()> {
    const ROUND_TRIP: Duration = Duration::from_millis(50);
    let dir = tempfile::tempdir()?;
    let files = crate::sftp_test_server::Files::seeded();
    let payload: Vec<u8> = (0..4 * 1024 * 1024).map(|i: u32| (i % 251) as u8).collect();
    files.insert("data.bin", payload.clone()).await;
    let chunks = (payload.len() / super::endpoint::CHUNK) as u32;
    let sequential = ROUND_TRIP * chunks;
    let remote = initialized(fixture_remote(&files, ROUND_TRIP).await).await?;
    // Downloads prefetch reads. Short replies are filled in before later chunks.
    for (name, short) in [("short", 20 * 1024), ("downloaded", 0)] {
        files.short_reads(short);
        let target = dir.path().join(name);
        let mut job = fixture(dir.path(), &target, Path::new("/data.bin")).await?;
        job.request.direction = Direction::Download;
        let started = tokio::time::Instant::now();
        job.execute(
            dir.path(),
            &remote,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(tokio::fs::read(&target).await?, payload);
        assert!(
            short > 0 || started.elapsed() < sequential / 2,
            "download took {:?}; one chunk per round trip takes {sequential:?}",
            started.elapsed()
        );
    }
    let target = dir.path().join("downloaded");
    // Uploads keep writes in flight; resuming verifies the remote prefix the same way.
    let mut job = fixture(dir.path(), &target, Path::new("/uploaded.bin")).await?;
    let cancel = CancellationToken::new();
    let pausing = cancel.clone();
    let p = progress();
    let mut rx = p.subscribe();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            if rx.borrow().bytes >= 1024 * 1024 {
                pausing.cancel();
                break;
            }
        }
    });
    job.execute(dir.path(), &Endpoint::Local, &remote, &p, &cancel)
        .await?;
    assert_eq!(job.state, State::Paused);
    let started = tokio::time::Instant::now();
    job.execute(
        dir.path(),
        &Endpoint::Local,
        &remote,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(job.state, State::Complete);
    assert_eq!(
        files.snapshot().await.get("uploaded.bin"),
        Some(&payload),
        "the resumed upload publishes the exact bytes"
    );
    assert!(
        started.elapsed() < sequential / 2,
        "verified resume took {:?}; one chunk per round trip takes {sequential:?}",
        started.elapsed()
    );
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn slow_links_keep_every_request_inside_the_request_timeout() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let files = crate::sftp_test_server::Files::seeded();
    // About 13 KiB/s: one 32 KiB read takes 2.5 s, which one request at a time
    // always handled. Sixteen queued requests would wait 40 s for the last reply.
    files.slow_reads(Duration::from_millis(2500));
    let remote = initialized(fixture_remote(&files, Duration::ZERO).await).await?;
    if let Endpoint::Remote(session) = &remote {
        session.set_timeout(30);
    }
    let target = dir.path().join("downloaded");
    let mut job = fixture(dir.path(), &target, Path::new("/large.bin")).await?;
    job.request.direction = Direction::Download;
    job.execute(
        dir.path(),
        &remote,
        &Endpoint::Local,
        &progress(),
        &CancellationToken::new(),
    )
    .await?;
    assert_eq!(job.state, State::Complete);
    assert_eq!(tokio::fs::read(&target).await?, vec![42; 4 * 1024 * 1024]);
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn read_only_directories_are_published_with_their_mode() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("src");
    std::fs::create_dir_all(source.join("nested"))?;
    std::fs::write(source.join("nested/a.txt"), b"x")?;
    for path in [source.join("nested"), source.clone()] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))?;
    }
    let target = dir.path().join("copy");
    let mut job = fixture(dir.path(), &source, &target).await?;
    let result = job
        .execute(
            dir.path(),
            &Endpoint::Local,
            &Endpoint::Local,
            &progress(),
            &CancellationToken::new(),
        )
        .await;
    let modes = [target.clone(), target.join("nested")].map(|path| {
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .ok()
    });
    // Let the temporary directory be removed.
    for path in [
        source.clone(),
        source.join("nested"),
        target.clone(),
        target.join("nested"),
    ] {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
    result?;
    assert_eq!(job.state, State::Complete);
    assert_eq!(modes, [Some(0o555), Some(0o555)]);
    assert_eq!(tokio::fs::read(target.join("nested/a.txt")).await?, b"x");
    assert!(!dir.path().join(format!(".nutty-{}.part", job.id)).exists());
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn local_modes_from_a_peer_respect_the_umask_and_drop_special_bits() -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let dir = tempfile::tempdir()?;
    // What the OS grants a new file asked for 0o777, measured independently.
    let probe = dir.path().join("probe");
    let allowed = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o777)
        .open(&probe)?
        .metadata()?
        .permissions()
        .mode()
        & 0o777;
    let target = dir.path().join("file");
    std::fs::write(&target, b"x")?;
    let metadata = super::endpoint::Metadata {
        kind: super::endpoint::Kind::File,
        size: 1,
        modified: None,
        mode: Some(0o4777),
    };
    Endpoint::Local
        .permissions(target.to_str().context("path")?, &metadata)
        .await?;
    assert_eq!(
        std::fs::metadata(&target)?.permissions().mode() & 0o7777,
        allowed
    );
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn a_job_that_cannot_be_saved_fails_alone_and_the_queue_keeps_running() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("transfers");
    let mut job = fixture(&root, &dir.path().join("local"), Path::new("/remote")).await?;
    job.state = State::Paused;
    job.save(&root).await?;
    let (prompts, _) = async_channel::bounded(1);
    let registry = Arc::new(crate::remote::Registry::new(dir.path(), prompts));
    let configuration = Configuration {
        connections: Vec::new(),
        proxies: Vec::new(),
        concurrency: 1,
        limit: 0,
    };
    let (queue, mut updates) = Queue::start(dir.path().into(), registry, configuration);
    let mut wait = async |predicate: &dyn Fn(&Snapshot) -> bool| {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.borrow_and_update().clone();
                if predicate(&snapshot) {
                    return Ok::<_, anyhow::Error>(snapshot);
                }
                updates.changed().await?;
            }
        })
        .await?
    };
    wait(&|s| s.tasks.len() == 1).await?;
    let folder = job.folder(&root);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500))?;
    queue.action(job.id, Action::Resume)?;
    let snapshot = wait(&|s| s.tasks.iter().any(|t| t.state == State::Failed)).await;
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    let snapshot = snapshot?;
    assert_eq!(snapshot.error, None, "the queue itself stays available");
    assert!(
        snapshot.tasks[0]
            .error
            .as_ref()
            .is_some_and(|e| e.starts_with("无法保存传输状态"))
    );
    // Later commands still reach the same queue.
    queue.action(job.id, Action::Cancel)?;
    wait(&|s| s.tasks.first().is_some_and(|t| t.state == State::Canceled)).await?;
    queue.action(job.id, Action::Remove)?;
    wait(&|s| s.tasks.is_empty()).await?;
    assert!(!folder.exists());
    queue.shutdown().await;
    Ok(())
}
