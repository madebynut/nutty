//! Local file publishing helpers shared by downloads and terminal file receives.

use std::{io, path::Path};

/// Publishes a completed temporary file at `target` without replacing an existing file.
///
/// A hard link is atomic and never clobbers. FAT32, exFAT and several network or FUSE
/// filesystems do not support hard links, so those fall back to an exclusive rename.
/// On success the temporary path no longer exists.
pub fn publish_new(temporary: &Path, target: &Path) -> io::Result<()> {
    match std::fs::hard_link(temporary, target) {
        Ok(()) => std::fs::remove_file(temporary),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        Err(link_error) => match rename_new(temporary, target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
            // Report the original failure when no fallback is available either.
            Err(_) => Err(link_error),
        },
    }
}

fn rename_new(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: Both C strings outlive the call; the flags forbid replacing any target.
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
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let source = wide(source);
        let target = wide(target);
        // SAFETY: NUL-terminated UTF-16 buffers live through the call. Without
        // MOVEFILE_REPLACE_EXISTING an existing target is rejected.
        let moved = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (source, target);
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_moves_the_file_and_never_replaces_a_target() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let temporary = directory.path().join("part");
        let target = directory.path().join("done");
        std::fs::write(&temporary, b"first")?;
        publish_new(&temporary, &target)?;
        assert!(!temporary.exists());
        assert_eq!(std::fs::read(&target)?, b"first");
        std::fs::write(&temporary, b"second")?;
        let error = publish_new(&temporary, &target).expect_err("target exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target)?, b"first");
        Ok(())
    }

    #[test]
    fn exclusive_rename_fallback_refuses_existing_targets() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        std::fs::write(&source, b"new")?;
        std::fs::write(&target, b"old")?;
        let error = rename_new(&source, &target).expect_err("target exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target)?, b"old");
        std::fs::remove_file(&target)?;
        rename_new(&source, &target)?;
        assert!(!source.exists());
        assert_eq!(std::fs::read(&target)?, b"new");
        Ok(())
    }
}
