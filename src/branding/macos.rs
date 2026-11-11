//! A lightweight native bundle for Cargo/direct GUI launches. CLI operations never re-exec.
use anyhow::{Context as _, Result, ensure};
use std::{
    fs,
    io::Write as _,
    os::unix::{
        fs::{MetadataExt as _, OpenOptionsExt as _},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
};

pub(super) fn prepare_launch() -> Result<()> {
    let executable = std::env::current_exe().context("locating Nutty executable")?;
    if in_bundle(&executable) {
        return Ok(());
    }
    let binary = runtime_bundle(&executable)?;
    // No runtime, PTYs or credentials have been opened yet. exec preserves the PID,
    // arguments and environment while registering the actual .app executable path.
    Err(std::process::Command::new(binary)
        .args(std::env::args_os().skip(1))
        .exec())
    .context("starting Nutty with its native application identity")
}

fn in_bundle(executable: &Path) -> bool {
    executable.parent().is_some_and(|p| {
        p.ends_with("Contents/MacOS")
            && p.parent()
                .and_then(Path::parent)
                .is_some_and(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
    })
}

fn runtime_bundle(executable: &Path) -> Result<PathBuf> {
    let directory = executable
        .parent()
        .context("Nutty executable has no parent directory")?;
    let bundle = directory.join("Nutty.runtime.app");
    for path in [
        &bundle,
        &bundle.join("Contents"),
        &bundle.join("Contents/MacOS"),
        &bundle.join("Contents/Resources"),
    ] {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    fs::symlink_metadata(path)?.is_dir(),
                    "runtime bundle contains a non-directory or symbolic link"
                );
            }
            Err(error) => return Err(error).context(
                "creating Nutty runtime bundle; use the packaged Nutty.app in read-only locations",
            ),
        }
    }
    write_resource(
        &bundle.join("Contents/Info.plist"),
        include_bytes!("../../assets/Info.plist"),
    )?;
    write_resource(
        &bundle.join("Contents/Resources/Nutty.icns"),
        include_bytes!("../../assets/Nutty.icns"),
    )?;
    let target = bundle.join("Contents/MacOS/Nutty");
    let source = fs::metadata(executable)?;
    let current = fs::symlink_metadata(&target).ok();
    if !current.is_some_and(|current| {
        current.is_file() && current.dev() == source.dev() && current.ino() == source.ino()
    }) {
        let temporary = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        // A hard link adds no second binary copy and retains the compiler's signature.
        // Rename keeps already-running versions intact during an incremental rebuild.
        fs::hard_link(executable, &temporary).context("linking runtime executable")?;
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    Ok(target)
}

fn write_resource(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() == bytes.len() as u64)
        && fs::read(path).is_ok_and(|current| current == bytes)
    {
        return Ok(());
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_bundle_reuses_binary_and_updates_only_after_a_new_build() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("Nutty");
        fs::write(&executable, b"first-build")?;
        let target = runtime_bundle(&executable)?;
        assert!(in_bundle(&target));
        assert_eq!(
            fs::metadata(&target)?.ino(),
            fs::metadata(&executable)?.ino()
        );
        let first_inode = fs::metadata(&target)?.ino();
        runtime_bundle(&executable)?;
        assert_eq!(fs::metadata(&target)?.ino(), first_inode);
        let replacement = directory.path().join("next-build");
        fs::write(&replacement, b"second-build")?;
        fs::rename(replacement, &executable)?;
        runtime_bundle(&executable)?;
        assert_ne!(fs::metadata(&target)?.ino(), first_inode);
        assert_eq!(fs::read(target)?, b"second-build");
        Ok(())
    }
    #[test]
    fn runtime_bundle_rejects_redirected_resource_directories() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("Nutty");
        fs::write(&executable, b"fixture")?;
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), directory.path().join("Nutty.runtime.app"))?;
        assert!(runtime_bundle(&executable).is_err());
        assert!(fs::read_dir(outside.path())?.next().is_none());
        Ok(())
    }
}
