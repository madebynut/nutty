//! Pane-owned Zmodem interception and cancellable file I/O outside the render thread.

use super::{Command, Engine};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    borrow::Cow,
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zmodem2::{Action, Event, FileInfo, Position};

const QUEUE: usize = 64;
const MAX_FILE: u64 = u32::MAX as u64;
const MAX_FILES: usize = 128;
const ABORT: &[u8] =
    b"\x18\x18\x18\x18\x18\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}
#[derive(Clone, Debug)]
pub struct Status {
    pub id: Uuid,
    pub direction: Direction,
    pub file: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub cancelling: bool,
}
struct Transfer {
    status: Status,
    prompted: bool,
    queued: Vec<u8>,
    wire: Option<SyncSender<Vec<u8>>>,
    cancel: CancellationToken,
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[derive(Default)]
pub(super) struct Bridge {
    prefix: Vec<u8>,
    prefix_since: Option<Instant>,
    transfer: Option<Transfer>,
}
impl Bridge {
    pub fn status(&self) -> Option<Status> {
        self.transfer.as_ref().map(|t| t.status.clone())
    }
    pub fn prefix_pending(&self) -> bool {
        !self.prefix.is_empty()
    }
    pub fn expired_prefix(&mut self) -> Option<Vec<u8>> {
        if self
            .prefix_since
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(100))
            && self.transfer.is_none()
        {
            self.prefix_since = None;
            return Some(std::mem::take(&mut self.prefix));
        }
        None
    }
    pub fn busy(&self) -> bool {
        self.transfer.is_some()
    }
    pub fn cancel(&mut self) {
        self.transfer = None;
        self.prefix.clear();
        self.prefix_since = None;
    }
    pub fn request(&mut self) -> Option<(Uuid, Direction)> {
        let transfer = self.transfer.as_mut()?;
        if transfer.prompted {
            return None;
        }
        transfer.prompted = true;
        Some((transfer.status.id, transfer.status.direction))
    }
    pub fn intercept<'data>(&mut self, bytes: &'data [u8]) -> Result<Cow<'data, [u8]>> {
        if self.transfer.is_none()
            && self.prefix.is_empty()
            && bytes.last() != Some(&b'*')
            && memchr::memchr(0x18, bytes).is_none()
        {
            return Ok(Cow::Borrowed(bytes));
        }
        if let Some(transfer) = &mut self.transfer {
            if transfer.cancel.is_cancelled() {
                return Ok(Cow::Borrowed(&[]));
            }
            if let Some(wire) = &transfer.wire {
                for chunk in bytes.chunks(16 * 1024) {
                    wire.try_send(chunk.to_vec())
                        .context("Zmodem 接收队列已满或传输已结束")?;
                }
            } else {
                ensure!(
                    transfer.queued.len() + bytes.len() <= 1024 * 1024,
                    "Zmodem 等待选择文件超出缓存限制"
                );
                transfer.queued.extend_from_slice(bytes);
            }
            return Ok(Cow::Borrowed(&[]));
        }
        let mut data = std::mem::take(&mut self.prefix);
        data.extend_from_slice(bytes);
        let signatures: [&[u8]; 2] = [b"**\x18B00", b"**\x18B01"];
        if let Some((index, direction)) = data.windows(6).enumerate().find_map(|(i, s)| {
            if s == signatures[0] {
                Some((i, Direction::Receive))
            } else if s == signatures[1] {
                Some((i, Direction::Send))
            } else {
                None
            }
        }) {
            let queued = data.split_off(index);
            self.transfer = Some(Transfer {
                status: Status {
                    id: Uuid::new_v4(),
                    direction,
                    file: String::new(),
                    bytes: 0,
                    total: None,
                    cancelling: false,
                },
                prompted: false,
                queued,
                wire: None,
                cancel: CancellationToken::new(),
            });
        } else {
            let held = (1..6)
                .rev()
                .find(|n| signatures.iter().any(|s| data.ends_with(&s[..*n])))
                .unwrap_or(0);
            self.prefix = data.split_off(data.len() - held);
            self.prefix_since = (held > 0).then(Instant::now);
        }
        Ok(Cow::Owned(data))
    }
}

impl Engine {
    pub fn zmodem_prefix_pending(&self) -> bool {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .zmodem
            .prefix_pending()
    }
    pub fn flush_zmodem_prefix(&self) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        if let Some(bytes) = core.zmodem.expired_prefix() {
            if let Some(journal) = &core.journal {
                journal.output(&bytes);
            }
            let super::Core {
                term,
                parser,
                graphics,
                ..
            } = &mut *core;
            parser.advance(&mut super::screen::Screen { term, graphics }, &bytes);
            core.generation = core.generation.wrapping_add(1);
            core.automation_revision = core.automation_revision.wrapping_add(1);
            drop(core);
            let _ = self.updates.try_send(());
        }
    }

    pub fn zmodem_request(&self) -> Option<(Uuid, Direction)> {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .zmodem
            .request()
    }
    pub fn cancel_zmodem(&self) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let Some(transfer) = &mut core.zmodem.transfer else {
            return;
        };
        if transfer.cancel.is_cancelled() {
            return;
        }
        let id = transfer.status.id;
        transfer.cancel.cancel();
        transfer.status.cancelling = true;
        core.generation = core.generation.wrapping_add(1);
        if self
            .commands
            .try_send(Command::Input(ABORT.to_vec()))
            .is_ok()
            || self.commands.is_closed()
        {
            core.zmodem.cancel();
            drop(core);
            let _ = self.updates.try_send(());
            return;
        }
        drop(core);
        let _ = self.updates.try_send(());
        let engine = self.clone();
        std::thread::spawn(move || {
            loop {
                let mut core = engine.core.lock().expect("terminal state lock poisoned");
                if !core
                    .zmodem
                    .transfer
                    .as_ref()
                    .is_some_and(|t| t.status.id == id)
                {
                    break;
                }
                if engine
                    .commands
                    .try_send(Command::Input(ABORT.to_vec()))
                    .is_ok()
                    || engine.commands.is_closed()
                {
                    core.zmodem.cancel();
                    core.generation = core.generation.wrapping_add(1);
                    drop(core);
                    let _ = engine.updates.try_send(());
                    break;
                }
                drop(core);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
    pub fn start_zmodem(&self, id: Uuid, paths: Vec<PathBuf>) -> Result<()> {
        if !self
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .zmodem
            .transfer
            .as_ref()
            .is_some_and(|t| t.status.id == id)
        {
            return Ok(());
        }
        if paths.is_empty() {
            self.cancel_zmodem();
            return Ok(());
        }
        ensure!(paths.len() <= MAX_FILES, "Zmodem 每次最多发送 128 个文件");
        let (wire, incoming) = mpsc::sync_channel(QUEUE);
        let (direction, initial, cancel) = {
            let mut core = self.core.lock().expect("terminal state lock poisoned");
            let transfer = core
                .zmodem
                .transfer
                .as_mut()
                .filter(|t| t.status.id == id && t.wire.is_none())
                .context("Zmodem 请求已结束")?;
            transfer.wire = Some(wire);
            (
                transfer.status.direction,
                std::mem::take(&mut transfer.queued),
                transfer.cancel.clone(),
            )
        };
        let engine = self.clone();
        let write_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("nutty-zmodem".into())
            .spawn(move || {
                let result = run(
                    direction,
                    paths,
                    initial,
                    &incoming,
                    cancel,
                    |bytes| {
                        let mut command = Command::Input(bytes.to_vec());
                        loop {
                            ensure!(!write_cancel.is_cancelled(), "传输已取消");
                            let core = engine.core.lock().expect("terminal state lock poisoned");
                            ensure!(
                                core.zmodem
                                    .transfer
                                    .as_ref()
                                    .is_some_and(|t| t.status.id == id),
                                "传输已结束"
                            );
                            let result = engine.commands.try_send(command);
                            drop(core);
                            match result {
                                Ok(()) => break Ok(()),
                                Err(async_channel::TrySendError::Closed(_)) => bail!("终端已关闭"),
                                Err(async_channel::TrySendError::Full(next)) => {
                                    command = next;
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                            }
                        }
                    },
                    |file, bytes, total| {
                        let mut core = engine.core.lock().expect("terminal state lock poisoned");
                        if let Some(t) = core.zmodem.transfer.as_mut().filter(|t| t.status.id == id)
                        {
                            t.status.file = file.to_string();
                            t.status.bytes = bytes;
                            t.status.total = total;
                            core.generation = core.generation.wrapping_add(1);
                        }
                        drop(core);
                        let _ = engine.updates.try_send(());
                    },
                );
                let mut core = engine.core.lock().expect("terminal state lock poisoned");
                let owns = core
                    .zmodem
                    .transfer
                    .as_ref()
                    .is_some_and(|t| t.status.id == id && !t.cancel.is_cancelled());
                if owns {
                    if let Err(error) = &result {
                        core.error = Some(format!(
                            "Zmodem：{}",
                            crate::i18n::localize(format!("{error:#}"))
                        ));
                    } else {
                        core.zmodem.cancel();
                        core.generation = core.generation.wrapping_add(1);
                    }
                }
                drop(core);
                if owns {
                    match result {
                        Ok(tail) => engine.feed(&tail),
                        Err(_) => engine.cancel_zmodem(),
                    }
                }
                let _ = engine.updates.try_send(());
            })?;
        Ok(())
    }
}

enum Machine {
    Send(zmodem2::Sender),
    Receive(zmodem2::Receiver),
}
impl Machine {
    fn poll(&mut self) -> Action<'_> {
        match self {
            Self::Send(s) => s.poll(),
            Self::Receive(r) => r.poll(),
        }
    }
    fn submit(&mut self, bytes: &[u8]) -> Result<usize> {
        Ok(match self {
            Self::Send(s) => s.submit_wire(bytes)?,
            Self::Receive(r) => r.submit_wire(bytes)?,
        })
    }
    fn written(&mut self, n: usize) {
        match self {
            Self::Send(s) => s.wire_written(n),
            Self::Receive(r) => r.wire_written(n),
        }
    }
    fn timeout(&mut self) -> Result<()> {
        match self {
            Self::Send(s) => s.timeout()?,
            Self::Receive(r) => r.timeout()?,
        };
        Ok(())
    }
}
struct IncomingFile {
    file: File,
    temporary: PathBuf,
    target: PathBuf,
    bytes: u64,
    size: Option<u64>,
}
impl Drop for IncomingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temporary);
    }
}
fn safe_name(bytes: &[u8]) -> Result<&str> {
    let name = std::str::from_utf8(bytes).context("Zmodem 文件名不是 UTF-8")?;
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && name != "."
            && name != ".."
            && !name.contains(['/', '\\', '\0', '\r', '\n']),
        "Zmodem 文件名无效"
    );
    Ok(name)
}
fn incoming_file(root: &Path, name: &[u8], size: Option<u64>) -> Result<IncomingFile> {
    let target = root.join(safe_name(name)?);
    ensure!(!target.try_exists()?, "接收文件已存在，请选择其他目录");
    ensure!(
        size.is_none_or(|s| s <= MAX_FILE),
        "文件超过 Zmodem 大小限制"
    );
    let temporary = root.join(format!(".nutty-{}.zmodem-part", Uuid::new_v4()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    Ok(IncomingFile {
        file,
        temporary,
        target,
        bytes: 0,
        size,
    })
}
fn next_file(
    sender: &mut zmodem2::Sender,
    paths: &mut VecDeque<PathBuf>,
) -> Result<Option<(File, String, u64)>> {
    let Some(path) = paths.pop_front() else {
        sender.finish()?;
        return Ok(None);
    };
    let name = safe_name(
        path.file_name()
            .and_then(|s| s.to_str())
            .context("文件名无效")?
            .as_bytes(),
    )?
    .to_string();
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_FILE,
        "Zmodem 仅支持不超过 4 GiB 的普通文件"
    );
    sender.start_file(FileInfo::new(
        name.as_bytes(),
        Some(Position::new(metadata.len() as u32)),
    ))?;
    Ok(Some((file, name, metadata.len())))
}
fn run(
    direction: Direction,
    paths: Vec<PathBuf>,
    initial: Vec<u8>,
    incoming: &Receiver<Vec<u8>>,
    cancel: CancellationToken,
    mut write: impl FnMut(&[u8]) -> Result<()>,
    mut progress: impl FnMut(&str, u64, Option<u64>),
) -> Result<Vec<u8>> {
    let mut paths: VecDeque<_> = paths.into();
    let root = paths.front().cloned().context("请选择文件或目录")?;
    let mut machine = match direction {
        Direction::Send => Machine::Send(zmodem2::Sender::new()?),
        Direction::Receive => {
            ensure!(root.is_dir(), "请选择接收目录");
            let mut r = zmodem2::Receiver::new()?;
            r.set_manual_file_accept(true);
            Machine::Receive(r)
        }
    };
    let mut source = if let Machine::Send(s) = &mut machine {
        next_file(s, &mut paths)?
    } else {
        None
    };
    let mut output: Option<IncomingFile> = None;
    let mut pending = initial;
    let mut offset = 0;
    let mut count = 0;
    let mut last = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        ensure!(!cancel.is_cancelled(), "传输已取消");
        match machine.poll() {
            Action::WriteWire(bytes) => {
                let n = bytes.len();
                write(bytes)?;
                machine.written(n);
            }
            Action::ReadFile { offset, max_len } => {
                let (file, name, total) = source.as_mut().context("发送文件未打开")?;
                file.seek(SeekFrom::Start(u64::from(offset.get())))?;
                let mut data = vec![0; max_len.min(64 * 1024)];
                let n = file.read(&mut data)?;
                ensure!(
                    n > 0 || u64::from(offset.get()) >= *total,
                    "发送期间文件被截断"
                );
                if let Machine::Send(s) = &mut machine {
                    s.submit_file(&data[..n])?;
                }
                if last_progress.elapsed() >= Duration::from_millis(80) {
                    progress(name, u64::from(offset.get()) + n as u64, Some(*total));
                    last_progress = Instant::now();
                }
            }
            Action::WriteFile(bytes) => {
                let file = output.as_mut().context("接收文件未打开")?;
                ensure!(
                    file.bytes + bytes.len() as u64 <= MAX_FILE
                        && file
                            .size
                            .is_none_or(|size| file.bytes + bytes.len() as u64 <= size),
                    "接收数据超过文件大小限制"
                );
                file.file.write_all(bytes)?;
                let n = bytes.len();
                file.bytes += n as u64;
                if let Machine::Receive(r) = &mut machine {
                    r.file_written(n)?;
                }
                if last_progress.elapsed() >= Duration::from_millis(80) {
                    progress(
                        file.target
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default(),
                        file.bytes,
                        file.size,
                    );
                    last_progress = Instant::now();
                }
            }
            Action::Event(Event::FileStarted(info)) => {
                count += 1;
                ensure!(count <= MAX_FILES, "Zmodem 每次最多接收 128 个文件");
                output = Some(incoming_file(
                    &root,
                    info.name,
                    info.size.map(|s| u64::from(s.get())),
                )?);
                let name = safe_name(info.name)?.to_string();
                let size = info.size.map(|s| u64::from(s.get()));
                if let Machine::Receive(r) = &mut machine {
                    r.accept_file_at(0)?;
                }
                progress(&name, 0, size);
            }
            Action::Event(Event::FileCompleted) => {
                if let Some(file) = output.take() {
                    ensure!(
                        file.size.is_none_or(|size| file.bytes == size),
                        "Zmodem 文件大小不完整"
                    );
                    file.file.sync_all()?;
                    ensure!(!cancel.is_cancelled(), "传输已取消");
                    std::fs::hard_link(&file.temporary, &file.target)
                        .context("无法保存接收文件，目标可能已存在")?;
                    progress(
                        file.target
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default(),
                        file.bytes,
                        file.size,
                    );
                }
                if let Machine::Send(s) = &mut machine {
                    source = next_file(s, &mut paths)?;
                }
            }
            Action::Event(Event::SessionCompleted) => {
                // The protocol reports completion before its final wire reply.
                while let Action::WriteWire(bytes) = machine.poll() {
                    let n = bytes.len();
                    write(bytes)?;
                    machine.written(n);
                }
                let mut tail = pending[offset..].to_vec();
                if direction == Direction::Receive {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !tail.windows(2).any(|s| s == b"OO") && Instant::now() < deadline {
                        match incoming.recv_timeout(Duration::from_millis(200)) {
                            Ok(bytes) => tail.extend(bytes),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if cancel.is_cancelled() {
                                    break;
                                }
                            }
                        }
                        ensure!(tail.len() <= 64 * 1024, "Zmodem 结束帧超过限制");
                    }
                    if let Some(end) = tail.windows(2).position(|s| s == b"OO") {
                        tail.drain(..end + 2);
                    }
                }
                for bytes in incoming.try_iter() {
                    if tail.len() + bytes.len() <= 1024 * 1024 {
                        tail.extend(bytes);
                    }
                }
                return Ok(tail);
            }
            Action::Event(Event::Aborted) => bail!("远端取消了 Zmodem 传输"),
            Action::Idle => {
                if offset < pending.len() {
                    let used = machine.submit(&pending[offset..])?;
                    ensure!(used > 0, "Zmodem 协议无法继续解析");
                    offset += used;
                    last = Instant::now();
                } else {
                    match incoming.recv_timeout(Duration::from_millis(200)) {
                        Ok(bytes) => {
                            pending = bytes;
                            offset = 0;
                            last = Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("终端已关闭"),
                        Err(mpsc::RecvTimeoutError::Timeout)
                            if last.elapsed() >= Duration::from_secs(10) =>
                        {
                            machine.timeout()?;
                            last = Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
            _ => bail!("不支持的 Zmodem 协议事件"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detection_survives_every_split_and_blocks_pane_input() -> Result<()> {
        let bytes = b"before**\x18B00000000000000\r\n\x11";
        for split in 0..=bytes.len() {
            let mut bridge = Bridge::default();
            let mut plain = bridge.intercept(&bytes[..split])?.into_owned();
            plain.extend_from_slice(&bridge.intercept(&bytes[split..])?);
            assert_eq!(plain, b"before", "split {split}");
            assert_eq!(bridge.request().map(|(_, d)| d), Some(Direction::Receive));
            assert!(bridge.request().is_none());
        }
        let (terminal, wire) = super::super::Terminal::channel(
            &crate::config::Preferences::default(),
            "zmodem".into(),
        );
        terminal.engine.feed(bytes);
        assert!(terminal.engine.input(b"echo hi\r".to_vec()).is_err());
        terminal.engine.cancel_zmodem();
        assert!(matches!(wire.recv_blocking()?, Command::Input(_)));
        terminal.engine.input(b"hello".to_vec())?;
        assert!(terminal.engine.snapshot().transfer.is_none());
        Ok(())
    }
    #[test]
    fn incomplete_signature_becomes_visible_and_old_selection_cannot_cancel_new_transfer()
    -> Result<()> {
        let (terminal, _) = super::super::Terminal::channel(
            &crate::config::Preferences::default(),
            "prefix".into(),
        );
        terminal.engine.feed(b"ordinary*");
        std::thread::sleep(Duration::from_millis(110));
        terminal.engine.flush_zmodem_prefix();
        assert_eq!(terminal.engine.recent_output().trim(), "ordinary*");
        terminal.engine.feed(b"**\x18B00000000000000\r\n");
        let old = terminal.engine.zmodem_request().context("first request")?.0;
        terminal.engine.cancel_zmodem();
        terminal.engine.feed(b"**\x18B00000000000000\r\n");
        let new = terminal
            .engine
            .zmodem_request()
            .context("second request")?
            .0;
        assert_ne!(old, new);
        terminal.engine.start_zmodem(old, Vec::new())?;
        assert_eq!(terminal.engine.snapshot().transfer.map(|t| t.id), Some(new));
        terminal.engine.cancel_zmodem();
        Ok(())
    }
    #[test]
    fn cancellation_keeps_input_locked_until_abort_is_queued() -> Result<()> {
        let (terminal, wire) = super::super::Terminal::channel(
            &crate::config::Preferences::default(),
            "cancel".into(),
        );
        for _ in 0..64 {
            terminal.engine.input(b"data".to_vec())?;
        }
        terminal.engine.feed(b"**\x18B00000000000000\r\n");
        terminal.engine.cancel_zmodem();
        assert!(
            terminal
                .engine
                .snapshot()
                .transfer
                .is_some_and(|t| t.cancelling)
        );
        assert!(terminal.engine.input(b"must-wait".to_vec()).is_err());
        wire.recv_blocking()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        while terminal.engine.snapshot().transfer.is_some() {
            ensure!(Instant::now() < deadline, "abort did not get queued");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut last = Vec::new();
        while let Ok(command) = wire.try_recv() {
            if let Command::Input(bytes) = command {
                last = bytes;
            }
        }
        assert_eq!(last, ABORT);
        terminal.engine.input(b"ready".to_vec())?;
        Ok(())
    }
    #[test]
    fn binary_files_round_trip_and_publish_without_partial_artifacts() -> Result<()> {
        let source = tempfile::tempdir()?;
        let destination = tempfile::tempdir()?;
        let payload: Vec<u8> = (0..400_000).map(|i| (i % 256) as u8).collect();
        let paths = [
            source.path().join("binary.dat"),
            source.path().join("中文.txt"),
        ];
        for path in &paths {
            std::fs::write(path, &payload)?;
        }
        let (to_send, send_in) = mpsc::sync_channel(64);
        let (to_receive, receive_in) = mpsc::sync_channel(64);
        let cancel = CancellationToken::new();
        let watchdog = cancel.clone();
        let timer = std::thread::spawn(move || {
            for _ in 0..100 {
                if watchdog.is_cancelled() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            watchdog.cancel();
        });
        let sender_cancel = cancel.clone();
        let sender_paths = paths.to_vec();
        let sender = std::thread::spawn(move || {
            run(
                Direction::Send,
                sender_paths,
                Vec::new(),
                &send_in,
                sender_cancel,
                |bytes| {
                    for chunk in bytes.chunks(13) {
                        to_receive.send(chunk.to_vec())?;
                    }
                    Ok(())
                },
                |_, _, _| {},
            )
        });
        let result = run(
            Direction::Receive,
            vec![destination.path().to_path_buf()],
            Vec::new(),
            &receive_in,
            cancel.clone(),
            |bytes| {
                to_send.send(bytes.to_vec())?;
                Ok(())
            },
            |_, _, _| {},
        );
        let sent = sender.join().expect("sender thread completes");
        cancel.cancel();
        timer.join().expect("watchdog thread completes");
        result?;
        sent?;
        for path in &paths {
            assert_eq!(
                std::fs::read(
                    destination
                        .path()
                        .join(path.file_name().context("filename")?)
                )?,
                payload
            );
        }
        assert_eq!(std::fs::read_dir(destination.path())?.count(), 2);
        Ok(())
    }
    #[test]
    fn receive_rejects_unsafe_names_and_preserves_existing_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        for name in [
            b"../escape".as_slice(),
            b"/absolute",
            b"a\\b",
            b"..",
            b"bad\0file",
        ] {
            assert!(incoming_file(root.path(), name, None).is_err());
        }
        std::fs::write(root.path().join("existing"), b"keep")?;
        assert!(incoming_file(root.path(), b"existing", None).is_err());
        {
            let mut file = incoming_file(root.path(), b"cancelled", Some(100))?;
            file.file.write_all(b"partial")?;
        }
        assert_eq!(std::fs::read_dir(root.path())?.count(), 1);
        assert_eq!(std::fs::read(root.path().join("existing"))?, b"keep");
        Ok(())
    }
}
