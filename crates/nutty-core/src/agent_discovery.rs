//! Local Agent CLI discovery and explicit ACP launch recipes.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt as _;

#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub adapter: bool,
}

#[derive(Clone, Debug)]
pub struct Detected {
    pub name: &'static str,
    pub version: String,
    pub executable: PathBuf,
    pub launch: Option<Launch>,
}

pub fn search_directories() -> Vec<PathBuf> {
    let mut directories: Vec<_> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if cfg!(unix) {
        directories.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    if cfg!(windows) {
        if let Some(root) = std::env::var_os("APPDATA") {
            directories.push(PathBuf::from(root).join("npm"));
        }
        if let Some(root) = std::env::var_os("LOCALAPPDATA") {
            directories.push(PathBuf::from(root).join("Microsoft/WinGet/Links"));
        }
        if let Some(root) = std::env::var_os("ProgramFiles") {
            directories.push(PathBuf::from(root).join("nodejs"));
        }
    }
    if let Some(home) = dirs::home_dir() {
        directories.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".npm-global/bin"),
        ]);
        // macOS GUI apps often inherit a minimal PATH, unlike interactive shells.
        for root in [
            home.join(".nvm/versions/node"),
            home.join(".local/share/fnm/node-versions"),
            home.join(".local/share/mise/installs/node"),
        ] {
            if let Ok(entries) = root.read_dir() {
                let mut versions: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
                versions.sort();
                directories.extend(versions.into_iter().rev().map(|path| {
                    if root.ends_with("node-versions") {
                        path.join("installation/bin")
                    } else {
                        path.join("bin")
                    }
                }));
            }
        }
    }
    directories
}

fn executable(path: &Path) -> bool {
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

fn find(directories: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    directories
        .iter()
        .flat_map(|directory| {
            names.iter().flat_map(move |name| {
                if cfg!(windows) && Path::new(name).extension().is_none() {
                    // npm installs extensionless POSIX shims alongside .cmd launchers.
                    // Selecting the shim first makes CreateProcess fail on Windows.
                    vec![
                        directory.join(format!("{name}.exe")),
                        directory.join(format!("{name}.cmd")),
                        directory.join(format!("{name}.bat")),
                    ]
                } else {
                    vec![directory.join(name)]
                }
            })
        })
        .find(|path| executable(path))
}

pub fn cli_executable(names: &[&str]) -> Option<PathBuf> {
    find(&search_directories(), names)
}

pub fn codex_executable() -> Option<PathBuf> {
    find(&search_directories(), &["codex", "codex.exe"])
}

fn discover(directories: &[PathBuf]) -> Vec<Detected> {
    let npx = find(directories, &["npx", "npx.cmd"]);
    let mut detected = Vec::new();
    for (name, commands, adapter, package, native_args) in [
        (
            "Claude",
            &["claude", "claude.exe"][..],
            Some("claude-agent-acp"),
            Some("@zed-industries/claude-agent-acp@latest"),
            &[][..],
        ),
        (
            "Codex",
            &["codex", "codex.exe"][..],
            Some("codex-acp"),
            Some("@agentclientprotocol/codex-acp@latest"),
            &[][..],
        ),
        (
            "Cursor",
            &["cursor-agent", "agent", "cursor-agent.exe"][..],
            None,
            None,
            &["acp"][..],
        ),
        (
            "Gemini CLI",
            &["gemini", "gemini.cmd"][..],
            None,
            None,
            &["--experimental-acp"][..],
        ),
        (
            "OpenCode",
            &["opencode", "opencode.exe"][..],
            None,
            None,
            &["acp"][..],
        ),
    ] {
        let adapter = adapter.and_then(|name| find(directories, &[name]));
        let Some(program) = find(directories, commands).or_else(|| adapter.clone()) else {
            continue;
        };
        let launch = if let Some(adapter) = adapter {
            Some(Launch {
                program: adapter,
                args: Vec::new(),
                adapter: false,
            })
        } else if let Some(package) = package {
            npx.as_ref().map(|npx| Launch {
                program: npx.clone(),
                args: vec!["--yes".into(), package.into()],
                adapter: true,
            })
        } else {
            Some(Launch {
                program: program.clone(),
                args: native_args.iter().map(|arg| (*arg).into()).collect(),
                adapter: false,
            })
        };
        detected.push(Detected {
            name,
            executable: program,
            version: String::new(),
            launch,
        });
    }
    detected
}

/// Only `--version` and `--help` are used; discovery never starts an Agent session.
async fn inspect(program: &Path, args: &[&str]) -> Option<String> {
    let reading = async {
        let mut command = tokio::process::Command::new(program);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Ok(path) = std::env::join_paths(search_directories()) {
            command.env("PATH", path);
        }
        let mut child = command.spawn().ok()?;
        let mut output = Vec::new();
        child
            .stdout
            .take()?
            .take(8192)
            .read_to_end(&mut output)
            .await
            .ok()?;
        let status = child.wait().await.ok()?;
        status
            .success()
            .then(|| String::from_utf8_lossy(&output).trim().to_string())
    };
    tokio::time::timeout(Duration::from_secs(2), reading)
        .await
        .ok()
        .flatten()
}

pub async fn installed() -> Vec<Detected> {
    let mut detected = tokio::task::spawn_blocking(|| discover(&search_directories()))
        .await
        .unwrap_or_default();
    for agent in &mut detected {
        agent.version = inspect(&agent.executable, &["--version"])
            .await
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(100)
            .collect();
        if agent.name == "Cursor"
            && !inspect(&agent.executable, &["acp", "--help"])
                .await
                .is_some_and(|help| help.to_lowercase().contains("acp"))
        {
            agent.launch = None;
        }
    }
    detected.retain(|agent| {
        !(agent.name == "Cursor"
            && agent.launch.is_none()
            && agent
                .executable
                .file_name()
                .is_some_and(|name| name == "agent"))
    });
    detected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_cli_and_adapter_are_distinct_and_path_precedence_is_preserved()
    -> std::io::Result<()> {
        let root = tempfile::tempdir()?;
        for name in ["codex", "claude", "cursor-agent", "npx"] {
            let path = root.path().join(if cfg!(windows) {
                format!("{name}.cmd")
            } else {
                name.into()
            });
            std::fs::write(&path, "#!/bin/sh\n")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        let directories = [root.path().to_owned(), root.path().to_owned()];
        let result = discover(&directories);
        assert_eq!(
            result.len(),
            3,
            "one row per installed Agent, even when PATH repeats"
        );
        let codex = result
            .iter()
            .find(|agent| agent.name == "Codex")
            .expect("Codex fixture");
        let launch = codex.launch.as_ref().expect("npx adapter recipe");
        assert!(launch.adapter);
        assert!(
            launch
                .args
                .iter()
                .any(|arg| arg == "@agentclientprotocol/codex-acp@latest")
        );
        assert_ne!(
            launch.program, codex.executable,
            "Codex CLI is not an ACP server"
        );
        let cursor = result
            .iter()
            .find(|agent| agent.name == "Cursor")
            .expect("Cursor fixture");
        assert_eq!(
            cursor.launch.as_ref().expect("native ACP recipe").args,
            ["acp"]
        );
        std::fs::remove_file(
            root.path()
                .join(if cfg!(windows) { "npx.cmd" } else { "npx" }),
        )?;
        assert!(
            discover(&directories)
                .iter()
                .find(|agent| agent.name == "Codex")
                .expect("Codex still installed")
                .launch
                .is_none()
        );
        Ok(())
    }
}
