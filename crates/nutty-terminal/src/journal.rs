//! Pane-local command history and opt-in rotating archives written off the UI thread.

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::Write as _,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_LOG: u64 = 10 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct History {
    session: Uuid,
    time: u64,
    command: String,
    directory: Option<String>,
}
struct Record {
    path: PathBuf,
    bytes: Vec<u8>,
    limit: u64,
}
#[derive(Clone)]
pub struct Recorder {
    root: Arc<PathBuf>,
    id: Uuid,
    pub commands: bool,
    pub output: bool,
}
impl Recorder {
    pub fn new(id: Uuid, preferences: &crate::config::Preferences) -> Option<Self> {
        if !preferences.record_history && !preferences.record_sessions {
            return None;
        }
        crate::config::data_dir().ok().map(|root| Self {
            root: Arc::new(root.join("journals")),
            id,
            commands: preferences.record_history,
            output: preferences.record_sessions,
        })
    }
    pub fn command(&self, command: &str, directory: Option<&str>) {
        if !self.commands || command.trim().is_empty() || command.len() > 64 * 1024 {
            return;
        }
        let record = History {
            session: self.id,
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            command: command.to_string(),
            directory: directory.map(str::to_string),
        };
        if let Ok(mut bytes) = serde_json::to_vec(&record) {
            bytes.push(b'\n');
            queue(Record {
                path: self.root.join("commands.jsonl"),
                bytes,
                limit: 1024 * 1024,
            });
        }
    }
    pub fn output(&self, bytes: &[u8]) {
        if !self.output || bytes.is_empty() {
            return;
        }
        queue(Record {
            path: self.root.join(format!("{}.log", self.id)),
            bytes: bytes.to_vec(),
            limit: MAX_LOG,
        });
    }
}
enum Message {
    Record(Record),
    Flush(std::sync::mpsc::Sender<()>),
}
static QUEUE: OnceLock<std::sync::mpsc::SyncSender<Message>> = OnceLock::new();
fn queue(record: Record) {
    let sender = QUEUE.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Message>(256);
        std::thread::spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    Message::Record(record) => {
                        if let Err(error) = append(record) {
                            tracing::warn!(%error,"terminal journal write failed");
                        }
                    }
                    Message::Flush(reply) => {
                        let _ = reply.send(());
                    }
                }
            }
        });
        sender
    });
    if sender.try_send(Message::Record(record)).is_err() {
        tracing::warn!("terminal journal queue full; record dropped");
    }
}
/// Waits off the UI thread until queued records have reached their files.
pub fn flush() -> Result<()> {
    let Some(queue) = QUEUE.get() else {
        return Ok(());
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut message = Message::Flush(sender);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match queue.try_send(message) {
            Ok(()) => break,
            Err(std::sync::mpsc::TrySendError::Full(pending)) => {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "journal flush timed out"
                );
                message = pending;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                anyhow::bail!("journal writer stopped")
            }
        }
    }
    receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))?;
    Ok(())
}
fn append(record: Record) -> Result<()> {
    let directory = record
        .path
        .parent()
        .context("journal directory unavailable")?;
    fs::create_dir_all(directory)?;
    if fs::metadata(&record.path)
        .is_ok_and(|metadata| metadata.len() + record.bytes.len() as u64 > record.limit)
    {
        let previous = record.path.with_extension("previous");
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(&record.path, previous)?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&record.path)?;
    let bytes = &record.bytes[record.bytes.len().saturating_sub(record.limit as usize)..];
    file.write_all(bytes)?;
    Ok(())
}
/// Recent commands belonging to one live terminal, independent of disk recording.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub command: String,
    pub directory: Option<String>,
}

#[derive(Default)]
pub struct CommandHistory {
    entries: VecDeque<HistoryEntry>,
    bytes: usize,
}

impl CommandHistory {
    pub fn record(&mut self, command: &str, directory: Option<&str>) {
        // Bound both the number of rows and memory, including multi-line scripts.
        const MAX_ENTRIES: usize = 256;
        const MAX_BYTES: usize = 512 * 1024;
        let command = command.trim_end();
        if command.trim().is_empty() || command.len() > 64 * 1024 {
            return;
        }
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.command == command)
            && let Some(entry) = self.entries.remove(index)
        {
            self.bytes -= entry.command.len() + entry.directory.as_ref().map_or(0, String::len);
        }
        let entry = HistoryEntry {
            command: command.into(),
            directory: directory
                .filter(|path| path.len() <= 4096)
                .map(str::to_string),
        };
        self.bytes += entry.command.len() + entry.directory.as_ref().map_or(0, String::len);
        self.entries.push_front(entry);
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            if let Some(entry) = self.entries.pop_back() {
                self.bytes -= entry.command.len() + entry.directory.as_ref().map_or(0, String::len);
            }
        }
    }

    pub fn entries(&self) -> Vec<HistoryEntry> {
        self.entries.iter().cloned().collect()
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_commands_are_unique_bounded_and_keep_the_latest_directory() {
        let mut history = CommandHistory::default();
        history.record("pwd", Some("/a"));
        history.record("ls", Some("/b"));
        history.record("pwd", Some("/c"));
        assert_eq!(history.entries()[0].command, "pwd");
        assert_eq!(history.entries()[0].directory.as_deref(), Some("/c"));
        assert_eq!(history.entries().len(), 2);
        history.record(" ", None);
        history.record(&"x".repeat(65 * 1024), None);
        assert_eq!(history.entries().len(), 2);
        for index in 0..600 {
            history.record(&format!("{index} {}", "x".repeat(4000)), None);
        }
        assert!(history.bytes <= 512 * 1024);
        assert!(history.entries.len() <= 256);
        assert!(history.entries()[0].command.starts_with("599 "));
        history.clear();
        assert!(history.entries().is_empty());
    }
}
