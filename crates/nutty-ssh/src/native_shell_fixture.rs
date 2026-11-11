//! Real isolated interactive shells, carried over the loopback SSH test server.
use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize};
use std::{
    io::{Read as _, Write as _},
    sync::{Arc, Mutex},
};

pub(super) struct NativeShell {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _directory: tempfile::TempDir,
}
impl NativeShell {
    pub fn start(
        shell: &str,
        channel: russh::ChannelId,
        connection: russh::server::Handle,
        size: crate::terminal::TermSize,
    ) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        // Recompute the prompt every time, as real themes/frameworks do.
        let rc = match shell {
            "zsh" => "function precmd() { PROMPT='fixture@target:~%# '; }; HISTFILE=/dev/null\n",
            "bash" => "PROMPT_COMMAND='PS1=\"fixture@target:~$ \"'; HISTFILE=/dev/null\n",
            _ => "PS1='fixture@target:~$ '; HISTFILE=/dev/null\n",
        };
        let path = directory
            .path()
            .join(if shell == "zsh" { ".zshrc" } else { "rc" });
        std::fs::write(&path, rc)?;
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: size.rows,
            cols: size.columns,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(format!("/bin/{shell}"));
        command.env("HOME", directory.path());
        command.env("ZDOTDIR", directory.path());
        command.env("ENV", &path);
        command.env("SHELL", format!("/bin/{shell}"));
        command.env("TERM", "xterm-256color");
        command.cwd(directory.path());
        if shell == "bash" {
            command.args(["--noprofile", "--rcfile"]);
            command.arg(&path);
        }
        if shell == "zsh" {
            command.arg("-d");
        }
        command.arg("-i");
        let child = pair.slave.spawn_command(command)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let mut buffer = [0; 16 * 1024];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0
                    || runtime
                        .block_on(connection.data(channel, buffer[..n].to_vec()))
                        .is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            child,
            _directory: directory,
        })
    }
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        let bytes = data.to_vec();
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().expect("test PTY writer");
            writer.write_all(&bytes)?;
            writer.flush()
        })
        .await??;
        Ok(())
    }
}
impl Drop for NativeShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
