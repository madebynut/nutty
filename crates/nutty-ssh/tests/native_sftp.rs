#![cfg(target_os = "macos")]

use anyhow::Result;
use std::{os::unix::fs::PermissionsExt, process::Stdio};
#[cfg(target_os = "macos")]
#[tokio::test]
async fn atomic_save_preserves_permissions_under_server_umask() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.conf");
    std::fs::write(&path, "before\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    let mut server = tokio::process::Command::new("/usr/libexec/sftp-server")
        .args(["-u", "0077"])
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stream = tokio::io::join(
        server.stdout.take().expect("stdout"),
        server.stdin.take().expect("stdin"),
    );
    nutty_ssh::sftp_edit::save(
        stream,
        path.to_str().expect("temporary path"),
        "before\n",
        "after\n",
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
    assert_eq!(std::fs::read_to_string(&path)?, "after\n");
    assert_eq!(mode, 0o640);
    let _ = server.kill().await;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn denied_permission_restore_keeps_original_file_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.conf");
    std::fs::write(&path, "before\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    let mut server = tokio::process::Command::new("/usr/libexec/sftp-server")
        .args(["-u", "0077", "-P", "fsetstat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stream = tokio::io::join(
        server.stdout.take().expect("stdout"),
        server.stdin.take().expect("stdin"),
    );
    let result = nutty_ssh::sftp_edit::save(
        stream,
        path.to_str().expect("path"),
        "before\n",
        "after\n",
        &tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_err(),
        "unsupported metadata preservation must not silently replace the file"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "before\n");
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        std::fs::read_dir(directory.path())?.count(),
        1,
        "failed save cleans its temporary file"
    );
    let _ = server.kill().await;
    Ok(())
}
