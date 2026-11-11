//! Discover local shell executables without starting processes or changing user configuration.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

const NAMES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "nu",
    "pwsh",
    "pwsh.exe",
    "powershell.exe",
    "cmd.exe",
];

pub fn installed(current: &Path) -> Vec<PathBuf> {
    let mut directories: Vec<_> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if cfg!(unix) {
        directories
            .extend(["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    }
    if let Some(home) = dirs::home_dir() {
        directories.extend([home.join(".local/bin"), home.join(".cargo/bin")]);
    }
    if cfg!(windows) {
        if let Some(root) = std::env::var_os("SystemRoot") {
            let root = PathBuf::from(root);
            directories.extend([
                root.join("System32"),
                root.join("System32/WindowsPowerShell/v1.0"),
            ]);
        }
        if let Some(root) = std::env::var_os("ProgramFiles") {
            directories.push(PathBuf::from(root).join("PowerShell/7"));
        }
    }
    let registry = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    discover(current, &directories, &registry)
}

fn discover(current: &Path, directories: &[PathBuf], registry: &str) -> Vec<PathBuf> {
    let mut candidates: BTreeSet<_> = registry
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect();
    for directory in directories {
        candidates.extend(NAMES.iter().map(|name| {
            directory.join(if cfg!(windows) && !name.ends_with(".exe") {
                format!("{name}.exe")
            } else {
                (*name).to_owned()
            })
        }));
    }
    candidates.retain(|path| executable(path));
    // Imported/custom launchers must survive saving unrelated settings.
    if !current.as_os_str().is_empty() {
        candidates.insert(current.to_owned());
    }
    candidates.into_iter().collect()
}

pub fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_filters_executables_deduplicates_and_preserves_custom_preferences()
    -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        let shell = root.path().join("zsh");
        let ordinary = root.path().join("fish");
        std::fs::write(&shell, "#!/bin/sh\n")?;
        std::fs::write(&ordinary, "not executable")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))?;
            std::fs::set_permissions(&ordinary, std::fs::Permissions::from_mode(0o644))?;
        }
        let custom = root.path().join("my-shell");
        let registry = format!(
            "# installed shells\n{} # system shell\n{}\n/nonexistent/shell\n",
            shell.display(),
            shell.display()
        );
        let choices = discover(
            &custom,
            &[root.path().to_owned(), root.path().to_owned()],
            &registry,
        );
        assert_eq!(choices.iter().filter(|path| **path == shell).count(), 1);
        assert!(
            choices.contains(&custom),
            "custom preference must not be discarded"
        );
        assert!(!choices.contains(&PathBuf::from("/nonexistent/shell")));
        #[cfg(unix)]
        assert!(
            !choices.contains(&ordinary),
            "non-executable files are not shells"
        );
        Ok(())
    }
}
