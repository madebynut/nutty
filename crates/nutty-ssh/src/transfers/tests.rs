use super::endpoint::Endpoint;
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
        assert_eq!(
            tokio::fs::read_link(dest.join("link")).await?,
            PathBuf::from("中文/file")
        );
    }
    assert!(!dir.path().join(format!(".nutty-{}.part", j.id)).exists());
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
