//! Transport-independent terminal state and a bounded local PTY actor.

use std::{
    io::{Read as _, Write as _},
    sync::{Arc, Mutex},
    thread,
};

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Boundary, Column, Direction, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{
        Config, Term, TermMode,
        cell::{Cell, Flags},
        search::RegexSearch,
    },
    vte::ansi::{Color, Processor},
};
use anyhow::{Context as _, Result, ensure};
use async_channel::{Receiver, Sender};
use portable_pty::{CommandBuilder, PtySize};

use crate::{config::Preferences, theme::Palette};

mod bootstrap_echo;
mod graphics;
pub use graphics::{DisplayImage, Raster};
mod inline_images;
mod protocol;
mod screen;
mod shell;
pub mod zmodem;

/// One MiB accommodates large pastes while keeping each queued message bounded.
const MAX_INPUT: usize = 1024 * 1024;

#[derive(Debug)]
pub enum Command {
    Input(Vec<u8>),
    Resize(TermSize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub columns: u16,
    pub rows: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            columns: 100,
            rows: 30,
        }
    }
}
impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }
    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}
impl From<TermSize> for PtySize {
    fn from(value: TermSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.columns,
            ..Default::default()
        }
    }
}

#[derive(Clone)]
struct Listener(Sender<Event>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.0.try_send(event);
    }
}

struct Core {
    bootstrap_echo: Option<bootstrap_echo::Echo>,
    environment: crate::session_environment::Environment,
    zmodem: zmodem::Bridge,
    term: Term<Listener>,
    events: Receiver<Event>,
    parser: Processor,
    decoder: protocol::Decoder,
    shell: shell::Shell,
    graphics: graphics::Graphics,
    cell: (f32, f32),
    journal: Option<crate::journal::Recorder>,
    history: crate::journal::CommandHistory,
    id: uuid::Uuid,
    generation: u64,
    automation_revision: u64,
    title: String,
    closed: bool,
    paused: bool,
    awaiting_echo: bool,
    error: Option<String>,
    search: Option<(String, Point)>,
}

#[derive(Clone, Debug)]
pub struct DisplayCell {
    pub column: usize,
    pub row: usize,
    pub cell: Cell,
    pub selected: bool,
}

/// Identifies the observed target independently from its terminal transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryTarget {
    pub id: uuid::Uuid,
    pub name: String,
    pub transport_bound: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub transfer: Option<zmodem::Status>,
    pub cells: Vec<DisplayCell>,
    pub images: Vec<graphics::DisplayImage>,
    pub directory: Option<String>,
    pub directory_target: Option<DirectoryTarget>,
    pub cursor: Option<(usize, usize)>,
    pub cursor_style: alacritty_terminal::vte::ansi::CursorStyle,
    pub mode: TermMode,
    pub generation: u64,
    pub automation_revision: u64,
    pub title: String,
    pub closed: bool,
    pub error: Option<String>,
    pub rows: usize,
    pub columns: usize,
}

#[derive(Clone)]
pub struct Engine {
    core: Arc<Mutex<Core>>,
    updates: Sender<()>,
    commands: Sender<Command>,
}

pub struct Terminal {
    pub engine: Engine,
    pub updates: Receiver<()>,
}

impl Terminal {
    pub fn channel(preferences: &Preferences, title: String) -> (Self, Receiver<Command>) {
        let (commands, input) = async_channel::bounded(64);
        let (updates_tx, updates) = async_channel::bounded(1);
        let (events_tx, events) = async_channel::unbounded();
        let id = uuid::Uuid::new_v4();
        let engine = Engine {
            core: Arc::new(Mutex::new(Core {
                bootstrap_echo: None,
                environment: Default::default(),
                zmodem: Default::default(),
                term: Term::new(
                    Config {
                        scrolling_history: preferences.scrollback,
                        default_cursor_style: preferences.terminal.cursor_style(),
                        ..Default::default()
                    },
                    &TermSize::default(),
                    Listener(events_tx),
                ),
                events,
                parser: Processor::new(),
                decoder: protocol::Decoder::default(),
                shell: shell::Shell::new(preferences.track_directory),
                graphics: graphics::Graphics::default(),
                cell: (8., 21.),
                journal: crate::journal::Recorder::new(id, preferences),
                history: Default::default(),
                id,
                generation: 0,
                automation_revision: 0,
                title,
                closed: false,
                paused: false,
                awaiting_echo: false,
                error: None,
                search: None,
            })),
            updates: updates_tx,
            commands: commands.clone(),
        };
        (Self { engine, updates }, input)
    }

    pub fn local(preferences: &Preferences) -> Result<Self> {
        ensure!(preferences.directory.is_dir(), "工作目录不存在");
        let system = portable_pty::native_pty_system();
        let pair = system
            .openpty(TermSize::default().into())
            .context("creating local PTY")?;
        let mut command = CommandBuilder::new(&preferences.shell);
        if !crate::shell_integration::configure(&mut command, &preferences.shell)? && cfg!(unix) {
            command.arg("-l");
        }
        command.cwd(&preferences.directory);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "nutty");
        let mut child = pair
            .slave
            .spawn_command(command)
            .context("starting local shell")?;
        drop(pair.slave);
        let mut killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let master = pair.master;
        let title = preferences
            .shell
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (terminal, input) = Self::channel(preferences, title);
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::local(
                &preferences.shell,
            ));
        terminal
            .engine
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .shell
            .directory = Some(preferences.directory.display().to_string());
        terminal
            .engine
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .graphics
            .allow_files = true;
        let engine = terminal.engine.clone();
        thread::Builder::new()
            .name("nutty-pty-read".into())
            .spawn(move || {
                let mut buffer = [0; 16 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => engine.feed(&buffer[..count]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            engine.fail(format!("终端读取失败：{error}"));
                            break;
                        }
                    }
                }
                engine.finish();
            })?;
        let engine = terminal.engine.clone();
        thread::Builder::new()
            .name("nutty-pty-write".into())
            .spawn(move || {
                while let Ok(command) = input.recv_blocking() {
                    let result = match command {
                        Command::Input(bytes) => writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .map_err(anyhow::Error::from),
                        Command::Resize(size) => master.resize(size.into()),
                    };
                    if let Err(error) = result {
                        engine.fail(format!("终端写入失败：{error}"));
                        break;
                    }
                }
                // The writer actor owns cancellation; the reaper always collects the child.
                let _ = killer.kill();
            })?;
        thread::Builder::new()
            .name("nutty-pty-reap".into())
            .spawn(move || {
                let _ = child.wait();
            })?;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.engine.close_input();
    }
}

impl Engine {
    pub fn set_environment(&self, environment: crate::session_environment::Environment) {
        self.flush_bootstrap_echo();
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.environment = environment;
        core.shell.reset_location();
        core.shell.bootstrap_input_pending = false;
        core.shell.input_pending = false;
    }

    /// Prevents a second bootstrap from racing the hooks queued with a new SSH shell.
    pub fn remote_integration_started(&self, source: &str) -> String {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.shell.integration_started();
        let echo = bootstrap_echo::Echo::new(
            source,
            core.term.columns(),
            core.term.grid().cursor.point.column.0,
        );
        let wire = echo.wire().to_owned();
        core.bootstrap_echo = Some(echo);
        wire
    }

    /// Indicates whether the remote shell handshake still needs a bounded timeout check.
    pub fn remote_integration_pending(&self) -> bool {
        let core = self.core.lock().expect("terminal state lock poisoned");
        core.shell.integration_pending() || core.bootstrap_echo.is_some()
    }

    /// Command history and automatic sends require prompt markers from the active shell.
    pub fn shell_integration_active(&self) -> bool {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .shell
            .integrated
    }

    /// Seeds transport identity, so a gateway-to-target switch cannot retain its directory.
    pub fn set_remote_identity(&self, user: &str, host: &str) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let bound = core.environment.startup_shell.is_some();
        core.shell.seed_target(format!("{user}@{host}"), bound);
    }

    /// Reads target identity under the engine lock, including unrendered target changes.
    pub fn directory_target(&self) -> Option<DirectoryTarget> {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .shell
            .target
            .clone()
    }

    /// Queries the current SSH shell once per target, without entering command history.
    pub fn request_directory_tracking(&self) {
        self.feed(&[]); // Also expires any unacknowledged echo transaction.
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        if core.environment.target != crate::session_environment::Target::Ssh
            || core.closed
            || core.paused
            || core.awaiting_echo
            || core.shell.bootstrap_input_pending
            || core.zmodem.busy()
            || core.term.mode().contains(TermMode::ALT_SCREEN)
        {
            return;
        }
        let point = core.term.grid().cursor.point;
        let prompt: String = (0..point.column.0)
            .map(|column| core.term.grid()[point.line][Column(column)].c)
            .collect();
        let previous_directory = core.shell.directory.clone();
        let previous_target = core.shell.target.clone();
        if let Some(command) = core.shell.probe_command(&prompt) {
            let echo = bootstrap_echo::Echo::new(&command, core.term.columns(), point.column.0);
            if self
                .commands
                .try_send(Command::Input(echo.wire().as_bytes().to_vec()))
                .is_ok()
            {
                core.bootstrap_echo = Some(echo);
                core.awaiting_echo = true;
                core.shell.prompt = None;
                core.shell.running = true;
            } else {
                core.shell.probe_not_sent();
            }
        }
        if core.shell.directory != previous_directory || core.shell.target != previous_target {
            core.generation = core.generation.wrapping_add(1);
            drop(core);
            let _ = self.updates.try_send(());
        }
    }

    pub fn environment(&self) -> crate::session_environment::Environment {
        let core = self.core.lock().expect("terminal state lock poisoned");
        let mut environment = if let Some(observed) = &core.shell.environment {
            observed.clone()
        } else if core.environment.target == crate::session_environment::Target::Ssh
            && core
                .shell
                .target
                .as_ref()
                .is_some_and(|target| !target.transport_bound)
        {
            crate::session_environment::Environment::unknown_ssh()
        } else {
            core.environment.clone()
        };
        environment.last_observed_directory = core.shell.directory.clone();
        environment
    }

    pub fn close_input(&self) {
        self.cancel_zmodem();
        self.commands.close();
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .history
            .clear();
    }

    pub fn command_history(&self) -> Vec<crate::journal::HistoryEntry> {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .history
            .entries()
    }

    pub fn is_closed(&self) -> bool {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .closed
    }

    pub fn set_cell_size(&self, width: f32, height: f32) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.cell = (width.max(1.), height.max(1.));
    }
    pub fn apply_preferences(&self, preferences: &Preferences) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.term.set_options(Config {
            scrolling_history: preferences.scrollback,
            default_cursor_style: preferences.terminal.cursor_style(),
            ..Default::default()
        });
        core.journal = crate::journal::Recorder::new(core.id, preferences);
        core.shell.track_directory = preferences.track_directory;
        if !preferences.track_directory {
            core.shell.directory = None;
        }
    }

    pub async fn wait_input_closed(&self) {
        self.commands.closed().await;
    }
    /// Moves the parsed screen and history from a completed transport into its replacement.
    pub fn inherit_screen(&self, previous: &Engine) {
        if Arc::ptr_eq(&self.core, &previous.core) {
            return;
        }
        let mut old = previous.core.lock().expect("terminal state lock poisoned");
        if !old.closed {
            return;
        }
        let mut new = self.core.lock().expect("terminal state lock poisoned");
        std::mem::swap(&mut new.term, &mut old.term);
        std::mem::swap(&mut new.events, &mut old.events);
        std::mem::swap(&mut new.parser, &mut old.parser);
        std::mem::swap(&mut new.graphics, &mut old.graphics);
        new.graphics.allow_files = false;
        // Prefer new observations; retain the last directory only for unchanged metadata.
        let same_environment = new.environment == old.environment;
        let directory = new.shell.directory.take();
        let target = new.shell.target.take();
        new.shell.target = target;
        new.shell.integrated = false;
        new.shell.prompt = None;
        new.shell.input_pending = false;
        new.shell.directory = directory.or_else(|| {
            same_environment
                .then(|| old.shell.directory.clone())
                .flatten()
        });
        std::mem::swap(&mut new.journal, &mut old.journal);
        std::mem::swap(&mut new.history, &mut old.history);
        std::mem::swap(&mut new.id, &mut old.id);
        // A partial transport frame cannot continue on a new connection.
        new.decoder = protocol::Decoder::default();
        new.generation = old.generation.wrapping_add(1);
        new.automation_revision = old.automation_revision.wrapping_add(1);
        drop(new);
        drop(old);
        self.feed(b"\r\n\x1b[0m[reconnected]\r\n");
    }
    pub fn feed(&self, bytes: &[u8]) {
        let mut decodes = Vec::new();
        {
            let mut core = self.core.lock().expect("terminal state lock poisoned");
            let filtered = bootstrap_echo::Echo::filter(&mut core.bootstrap_echo, bytes);
            if filtered.is_empty() {
                return;
            }
            let bytes = match core.zmodem.intercept(&filtered) {
                Ok(bytes) => bytes,
                Err(error) => {
                    core.zmodem.cancel();
                    core.error = Some(format!("Zmodem：{error:#}"));
                    let _ = self
                        .commands
                        .try_send(Command::Input(b"\x18\x18\x18\x18\x18".to_vec()));
                    std::borrow::Cow::Borrowed(&[] as &[u8])
                }
            };
            if let Some(journal) = &core.journal {
                journal.output(&bytes);
            }
            let mut decoder = std::mem::take(&mut core.decoder);
            decoder.advance(&bytes, |part| match part {
                protocol::Part::Text(data) => {
                    let Core {
                        term,
                        parser,
                        graphics,
                        ..
                    } = &mut *core;
                    parser.advance(&mut screen::Screen { term, graphics }, data);
                }
                protocol::Part::Osc(data) => {
                    let cursor = core.term.grid().cursor.point;
                    core.shell.osc(data, cursor);
                    if data.starts_with(b"1337;") {
                        let alternate = core.term.mode().contains(TermMode::ALT_SCREEN);
                        let cell = core.cell;
                        let viewport = (core.term.columns(), core.term.screen_lines());
                        if let Ok(Some(outcome)) = core.graphics.iterm(
                            data,
                            (cursor.column.0, cursor.line.0),
                            alternate,
                            cell,
                            viewport,
                        ) {
                            apply_inline_image(&mut core, outcome, &mut decodes);
                        }
                        return;
                    }
                    let Core { term, parser, .. } = &mut *core;
                    parser.advance(term, b"\x1b]");
                    parser.advance(term, data);
                    parser.advance(term, b"\x07");
                }
                protocol::Part::Apc(data) => {
                    if let Some(data) = data.strip_prefix(b"G") {
                        let cursor = core.term.grid().cursor.point;
                        let alternate = core.term.mode().contains(TermMode::ALT_SCREEN);
                        let cell = core.cell;
                        let outcome = core.graphics.handle(
                            data,
                            (cursor.column.0, cursor.line.0),
                            alternate,
                            cell,
                        );
                        if let Some(response) = outcome.response {
                            let _ = self
                                .commands
                                .try_send(Command::Input(response.into_bytes()));
                        }
                        if let Some(decode) = outcome.decode {
                            decodes.push(decode);
                        }
                        if outcome.move_rows > 0 {
                            let rows = outcome.move_rows.min(core.term.screen_lines());
                            let Core { term, graphics, .. } = &mut *core;
                            let mut screen = screen::Screen { term, graphics };
                            use alacritty_terminal::vte::ansi::Handler as _;
                            for _ in 0..rows {
                                screen.linefeed();
                            }
                            screen.move_forward(outcome.move_columns);
                        }
                    }
                }
                protocol::Part::Dcs(data) => {
                    let q = data.iter().position(|byte| *byte == b'q');
                    if q.is_some_and(|q| {
                        data[..q]
                            .iter()
                            .all(|byte| byte.is_ascii_digit() || *byte == b';')
                    }) {
                        let cursor = core.term.grid().cursor.point;
                        let alternate = core.term.mode().contains(TermMode::ALT_SCREEN);
                        let cell = core.cell;
                        if let Ok(outcome) = core.graphics.sixel(
                            data,
                            (cursor.column.0, cursor.line.0),
                            alternate,
                            cell,
                        ) {
                            apply_inline_image(&mut core, outcome, &mut decodes);
                        }
                    } else {
                        let Core { term, parser, .. } = &mut *core;
                        parser.advance(term, b"\x1bP");
                        parser.advance(term, data);
                        parser.advance(term, b"\x1b\\");
                    }
                }
            });
            core.decoder = decoder;
            core.generation = core.generation.wrapping_add(1);
            core.automation_revision = core.automation_revision.wrapping_add(1);
            if !bytes.is_empty() {
                core.awaiting_echo = false;
            }
        }
        for decode in decodes {
            decode_image(
                decode,
                Arc::downgrade(&self.core),
                self.updates.clone(),
                self.commands.clone(),
            );
        }
        let events = self
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .events
            .clone();
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Title(title) => {
                    self.core
                        .lock()
                        .expect("terminal state lock poisoned")
                        .title = title
                }
                Event::PtyWrite(text) => {
                    let text = if text == "\x1b[?6c" {
                        "\x1b[?62;4c".to_string()
                    } else if text == "\x1b[?80;0$y" {
                        let mode = if self
                            .core
                            .lock()
                            .expect("terminal state lock poisoned")
                            .graphics
                            .sixel_display_mode
                        {
                            1
                        } else {
                            2
                        };
                        format!("\x1b[?80;{mode}$y")
                    } else {
                        text
                    };
                    if self
                        .commands
                        .try_send(Command::Input(text.into_bytes()))
                        .is_err()
                    {
                        self.fail("终端响应队列已满".into());
                    }
                }
                Event::TextAreaSizeRequest(format) => {
                    let core = self.core.lock().expect("terminal state lock poisoned");
                    let text = format(alacritty_terminal::event::WindowSize {
                        num_lines: core.term.screen_lines() as u16,
                        num_cols: core.term.columns() as u16,
                        cell_width: core.cell.0 as u16,
                        cell_height: core.cell.1 as u16,
                    });
                    drop(core);
                    let _ = self.commands.try_send(Command::Input(text.into_bytes()));
                }
                // OSC 52 has no implicit access to the system clipboard.
                _ => {}
            }
        }
        let _ = self.updates.try_send(());
    }

    pub fn snapshot(&self) -> Snapshot {
        let core = self.core.lock().expect("terminal state lock poisoned");
        let content = core.term.renderable_content();
        let offset = content.display_offset as i32;
        let rows = core.term.screen_lines();
        let columns = core.term.columns();
        let cells = content
            .display_iter
            .filter_map(|indexed| {
                let row = indexed.point.line.0 + offset;
                if row < 0 || row as usize >= rows {
                    return None;
                }
                let selected = content
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.contains(indexed.point));
                let mut cell = indexed.cell.clone();
                // Resolve dynamic OSC colors while the borrowed color table is available.
                for color in [&mut cell.fg, &mut cell.bg] {
                    let index = match *color {
                        Color::Indexed(i) => Some(usize::from(i)),
                        Color::Named(n) => Some(n as usize),
                        Color::Spec(_) => None,
                    };
                    if let Some(index) = index
                        && let Some(value) = content.colors[index]
                    {
                        *color = Color::Spec(value);
                    }
                }
                Some(DisplayCell {
                    column: indexed.point.column.0,
                    row: row as usize,
                    cell,
                    selected,
                })
            })
            .collect();
        let cursor_row = content.cursor.point.line.0 + offset;
        let cursor = (content.mode.contains(TermMode::SHOW_CURSOR)
            && cursor_row >= 0
            && (cursor_row as usize) < rows)
            .then_some((content.cursor.point.column.0, cursor_row.max(0) as usize));
        Snapshot {
            transfer: core.zmodem.status(),
            cells,
            images: core.graphics.snapshot(
                content.display_offset,
                content.mode.contains(TermMode::ALT_SCREEN),
            ),
            directory: core.shell.directory.clone(),
            directory_target: core.shell.target.clone(),
            cursor,
            cursor_style: core.term.cursor_style(),
            mode: content.mode,
            generation: core.generation,
            automation_revision: core.automation_revision,
            title: core.title.clone(),
            closed: core.closed,
            error: core.error.clone(),
            rows,
            columns,
        }
    }

    pub fn resize(&self, size: TermSize) -> Result<()> {
        ensure!(
            size.rows > 0 && size.columns > 1,
            "invalid terminal dimensions"
        );
        self.commands
            .try_send(Command::Resize(size))
            .context("terminal resize queue unavailable")?;
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let was_empty = !core.awaiting_echo && empty_prompt(&core);
        core.term.resize(size);
        if was_empty && core.shell.integrated {
            core.shell.prompt = Some(core.term.grid().cursor.point);
        }
        core.graphics.region = None;
        core.generation = core.generation.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
        Ok(())
    }

    pub fn input(&self, bytes: Vec<u8>) -> Result<()> {
        ensure!(bytes.len() <= MAX_INPUT, "输入内容超过 1 MiB");
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let editing = !bytes.is_empty();
        let submitted = bytes.last().is_some_and(|b| matches!(b, b'\r' | b'\n'));
        let empty_enter = matches!(bytes.as_slice(), b"\r" | b"\n");
        let enter = submitted && !bytes.starts_with(b"\x1b[200~");
        let interrupt = bytes == [3];
        let entered = if enter && core.shell.integrated && !core.shell.running {
            core.shell.prompt.map(|start| {
                core.term
                    .bounds_to_string(start, core.term.grid().cursor.point)
            })
        } else {
            None
        };
        ensure!(!core.closed, "终端已关闭");
        ensure!(!core.paused, "终端正在重连，请稍后输入");
        ensure!(
            !core.zmodem.busy(),
            "Zmodem 传输中，请等待完成或取消后再输入"
        );
        self.commands
            .try_send(Command::Input(bytes))
            .context("terminal input queue unavailable")?;
        if empty_enter {
            core.shell.retry_after_empty_enter();
        }
        if enter || interrupt {
            core.shell.input_pending = false;
        } else {
            core.shell.input_pending |= editing;
        }
        if editing {
            core.shell.bootstrap_input_pending = !submitted;
        }
        if let Some(command) = entered {
            record_command(&mut core, &command);
        }
        core.awaiting_echo = true;
        if core.shell.integrated && (enter || interrupt) {
            core.shell.running = true;
            core.shell.prompt = None;
        }
        core.term.scroll_display(Scroll::Bottom);
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
        Ok(())
    }

    pub fn paste(&self, text: &str) -> Result<()> {
        let bracketed = self
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .term
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        // Remove ESC inside bracketed paste so pasted input cannot terminate its frame.
        let bytes = if bracketed {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
        } else {
            text.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
        };
        self.input(bytes)
    }

    /// Sends one complete command submission without routing through broadcast input.
    pub fn send_command(&self, command: &str) -> Result<()> {
        self.submit_command(command, None)
    }

    /// Checks shell activity atomically with automatic command submission.
    pub fn send_automatic_command(&self, command: &str, revision: u64) -> Result<()> {
        self.submit_command(command, Some(revision))
    }

    fn submit_command(&self, command: &str, revision: Option<u64>) -> Result<()> {
        crate::command::validate(command)?;
        let command = command.replace("\r\n", "\n").replace('\r', "\n");
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        ensure!(!core.closed, "终端已关闭");
        ensure!(!core.paused, "终端正在重连，请稍后输入");
        ensure!(
            !core.zmodem.busy(),
            "Zmodem 传输中，请等待完成或取消后再输入"
        );
        ensure!(
            revision.is_none_or(|revision| core.automation_revision == revision),
            "终端状态已变化，请手动执行"
        );
        ensure!(
            !core.term.mode().contains(TermMode::ALT_SCREEN),
            "请先退出全屏程序，再发送 Shell 命令"
        );
        ensure!(
            !core.awaiting_echo
                && (empty_prompt(&core) || (revision.is_none() && visible_prompt(&core))),
            "请先回到空的 Shell 提示符，再执行命令"
        );
        let bracketed = core.term.mode().contains(TermMode::BRACKETED_PASTE);
        let bytes = if bracketed {
            format!("\x1b[200~{command}\x1b[201~\r").into_bytes()
        } else {
            format!("{}\r", command.replace('\n', "\r")).into_bytes()
        };
        self.commands
            .try_send(Command::Input(bytes))
            .context("terminal input queue unavailable")?;
        record_command(&mut core, &command);
        core.awaiting_echo = true;
        if core.shell.integrated {
            core.shell.running = true;
        }
        core.term.scroll_display(Scroll::Bottom);
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
        Ok(())
    }

    /// Reads recent parsed output, independent of the user's current scroll position.
    pub fn recent_output(&self) -> String {
        const LIMIT: usize = 32 * 1024;
        let core = self.core.lock().expect("terminal state lock poisoned");
        let bottom = core.term.screen_lines() as i32 - 1;
        let top = (bottom - 119).max(core.term.topmost_line().0);
        let mut lines = Vec::new();
        let mut bytes = 0;
        for row in (top..=bottom).rev() {
            let mut text = String::new();
            for column in 0..core.term.columns() {
                let cell = &core.term.grid()[Line(row)][Column(column)];
                if !cell.flags.contains(Flags::HIDDEN) {
                    text.push_str(&cell_text(cell));
                }
            }
            let text = text.trim_end();
            if bytes + text.len() + 1 > LIMIT {
                break;
            }
            lines.push(text.to_string());
            bytes += text.len() + 1;
        }
        lines.reverse();
        lines.join("\n").trim().to_string()
    }

    /// Automation requires an unchanged terminal and a recognisable empty prompt.
    pub fn automatic_command_ready(&self, revision: u64) -> bool {
        let core = self.core.lock().expect("terminal state lock poisoned");
        if core.closed
            || core.paused
            || core.awaiting_echo
            || core.zmodem.busy()
            || core.automation_revision != revision
            || core.term.mode().contains(TermMode::ALT_SCREEN)
        {
            return false;
        }
        empty_prompt(&core)
    }

    /// Clears screen output and scrollback while retaining the current shell input row.
    pub fn clear_screen(&self) -> Result<()> {
        use alacritty_terminal::vte::ansi::Handler as _;
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        ensure!(
            !core.term.mode().contains(TermMode::ALT_SCREEN),
            "请先退出全屏程序，再清屏"
        );
        let cursor = core.term.grid().cursor.point;
        let rows = core.term.screen_lines();
        core.term
            .grid_mut()
            .scroll_up(&(Line(0)..Line(rows as i32)), cursor.line.0.max(0) as usize);
        core.term.grid_mut().cursor.point.line = Line(0);
        core.term.grid_mut().reset_region(Line(1)..);
        core.term
            .clear_screen(alacritty_terminal::vte::ansi::ClearMode::Saved);
        core.term.scroll_display(Scroll::Bottom);
        if let Some(prompt) = &mut core.shell.prompt {
            prompt.line = Line((prompt.line.0 - cursor.line.0).max(0));
        }
        core.term.selection = None;
        core.search = None;
        let allow_files = core.graphics.allow_files;
        core.graphics = graphics::Graphics::default();
        core.graphics.allow_files = allow_files;
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
        Ok(())
    }

    pub fn scroll(&self, scroll: Scroll) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.term.scroll_display(scroll);
        core.generation = core.generation.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }

    pub fn select(&self, column: usize, row: usize, extend: bool, kind: SelectionType) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let point = Point::new(
            Line(
                row.min(core.term.screen_lines() - 1) as i32
                    - core.term.grid().display_offset() as i32,
            ),
            Column(column.min(core.term.columns() - 1)),
        );
        if extend {
            if let Some(selection) = &mut core.term.selection {
                selection.update(point, Side::Right);
            }
        } else {
            core.term.selection = Some(Selection::new(kind, point, Side::Left));
        }
        core.generation = core.generation.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }

    pub fn selection(&self) -> Option<String> {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .term
            .selection_to_string()
    }

    pub fn select_all(&self) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        let start = Point::new(Line(-(core.term.history_size() as i32)), Column(0));
        let end = Point::new(
            Line(core.term.screen_lines() as i32 - 1),
            Column(core.term.columns() - 1),
        );
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        core.term.selection = Some(selection);
        core.generation = core.generation.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }

    pub fn pause(&self) {
        self.flush_bootstrap_echo();
        self.cancel_zmodem();
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.paused = true;
        core.awaiting_echo = false;
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }
    pub fn resume(&self) {
        self.feed(b"\x1b[?1049l\r\n\x1b[0m[reconnected]\r\n");
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.paused = false;
        core.error = None;
        core.awaiting_echo = false;
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }
    pub fn finish(&self) {
        self.flush_bootstrap_echo();
        self.cancel_zmodem();
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.closed = true;
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }

    fn flush_bootstrap_echo(&self) {
        let bytes = self
            .core
            .lock()
            .expect("terminal state lock poisoned")
            .bootstrap_echo
            .take()
            .map(|mut echo| echo.flush());
        if let Some(bytes) = bytes {
            self.feed(&bytes);
        }
    }

    pub fn search(&self, query: &str, next: bool) -> Result<bool> {
        ensure!(query.len() <= 1024, "search expression exceeds 1024 bytes");
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        if query.is_empty() {
            core.search = None;
            core.term.selection = None;
            core.generation = core.generation.wrapping_add(1);
            drop(core);
            let _ = self.updates.try_send(());
            return Ok(false);
        }
        let mut regex =
            RegexSearch::new(query).map_err(|_| anyhow::anyhow!("invalid search expression"))?;
        let origin = core
            .search
            .as_ref()
            .filter(|(previous, _)| next && previous == query)
            .map(|(_, point)| point.add(&core.term, Boundary::Grid, 1))
            .unwrap_or_else(|| Point::new(core.term.topmost_line(), Column(0)));
        let Some(found) =
            core.term
                .search_next(&mut regex, origin, Direction::Right, Side::Left, None)
        else {
            return Ok(false);
        };
        let mut selection = Selection::new(SelectionType::Simple, *found.start(), Side::Left);
        selection.update(*found.end(), Side::Right);
        core.term.selection = Some(selection);
        core.term.scroll_to_point(*found.start());
        core.search = Some((query.to_string(), *found.end()));
        core.generation = core.generation.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
        Ok(true)
    }

    pub fn fail(&self, error: String) {
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        core.error = Some(error);
        core.generation = core.generation.wrapping_add(1);
        core.automation_revision = core.automation_revision.wrapping_add(1);
        drop(core);
        let _ = self.updates.try_send(());
    }
}

pub fn resolve_color(color: Color, palette: Palette, foreground: bool) -> u32 {
    match color {
        Color::Spec(rgb) => (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b),
        Color::Named(name) => match name as usize {
            0..16 => palette.ansi[name as usize],
            256 => palette.text,
            257 => palette.terminal,
            258 => palette.accent,
            _ => {
                if foreground {
                    palette.text
                } else {
                    palette.terminal
                }
            }
        },
        Color::Indexed(index) => match index {
            0..16 => palette.ansi[usize::from(index)],
            16..232 => {
                let value = index - 16;
                let level = |value: u8| {
                    if value == 0 {
                        0
                    } else {
                        u32::from(value) * 40 + 55
                    }
                };
                (level(value / 36) << 16) | (level(value / 6 % 6) << 8) | level(value % 6)
            }
            _ => {
                let value = u32::from(index - 232) * 10 + 8;
                value * 0x010101
            }
        },
    }
}

pub fn cell_text(cell: &Cell) -> String {
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        return String::new();
    }
    let mut text = cell.c.to_string();
    if let Some(extra) = cell.zerowidth() {
        text.extend(extra);
    }
    text
}

struct ImageJob {
    decode: graphics::Decode,
    core: std::sync::Weak<Mutex<Core>>,
    updates: Sender<()>,
    commands: Sender<Command>,
}
fn apply_inline_image(
    core: &mut Core,
    outcome: graphics::Outcome,
    decodes: &mut Vec<graphics::Decode>,
) {
    if let Some(decode) = outcome.decode {
        decodes.push(decode);
    }
    let mut screen = screen::Screen {
        term: &mut core.term,
        graphics: &mut core.graphics,
    };
    use alacritty_terminal::vte::ansi::Handler as _;
    for _ in 0..outcome.move_rows {
        screen.linefeed();
    }
    if outcome.move_rows > 0 {
        screen.carriage_return();
    }
}

fn decode_image(
    decode: graphics::Decode,
    core: std::sync::Weak<Mutex<Core>>,
    updates: Sender<()>,
    commands: Sender<Command>,
) {
    static QUEUE: std::sync::OnceLock<std::sync::mpsc::SyncSender<ImageJob>> =
        std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ImageJob>(8);
        std::thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                if job.core.strong_count() == 0 {
                    continue;
                }
                let permitted = job.core.upgrade().is_some_and(|core| {
                    let core = core.lock().expect("terminal state lock poisoned");
                    !core.closed && (!job.decode.reads_file() || core.graphics.allow_files)
                });
                let result = if permitted {
                    job.decode.pixels()
                } else {
                    Err(anyhow::anyhow!("image permission revoked"))
                };
                if let Some(core) = job.core.upgrade() {
                    let mut core = core.lock().expect("terminal state lock poisoned");
                    let cell = core.cell;
                    if let Some(response) = core.graphics.install(&job.decode, result, cell) {
                        let _ = job.commands.try_send(Command::Input(response.into_bytes()));
                    }
                    core.generation = core.generation.wrapping_add(1);
                    drop(core);
                    let _ = job.updates.try_send(());
                }
            }
        });
        sender
    });
    if let Err(
        std::sync::mpsc::TrySendError::Full(job) | std::sync::mpsc::TrySendError::Disconnected(job),
    ) = queue.try_send(ImageJob {
        decode,
        core,
        updates,
        commands,
    }) && let Some(core) = job.core.upgrade()
    {
        let mut core = core.lock().expect("terminal state lock poisoned");
        let cell = core.cell;
        if let Some(response) = core.graphics.install(
            &job.decode,
            Err(anyhow::anyhow!("image decoder busy")),
            cell,
        ) {
            let _ = job.commands.try_send(Command::Input(response.into_bytes()));
        }
    }
}

fn record_command(core: &mut Core, command: &str) {
    core.history
        .record(command, core.shell.directory.as_deref());
    if let Some(journal) = &core.journal {
        journal.command(command, core.shell.directory.as_deref());
    }
}

fn empty_prompt(core: &Core) -> bool {
    let point = core.term.grid().cursor.point;
    core.shell.integrated
        && !core.shell.running
        && !core.shell.input_pending
        && core.shell.prompt == Some(point)
}

// A gateway or nested SSH shell can stop emitting the outer shell's OSC markers.
// Explicit sends may use its visible prompt; automatic sends retain strict OSC checks.
fn visible_prompt(core: &Core) -> bool {
    let point = core.term.grid().cursor.point;
    let prompt: String = (0..point.column.0)
        .map(|column| core.term.grid()[point.line][Column(column)].c)
        .collect();
    prompt
        .trim_end()
        .ends_with(['$', '#', '%', '❯', '➜', '>', 'λ', '›'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_home_and_prompt_redraw_do_not_make_existing_input_empty() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "editing".into());
        terminal.engine.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        terminal.engine.input(b"printf pending".to_vec())?;
        let _ = input.try_recv()?;
        terminal.engine.feed(b"printf pending\r\x1b[2C");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(!terminal.engine.automatic_command_ready(revision));
        assert!(
            terminal
                .engine
                .send_automatic_command("'/bin/pwd'", revision)
                .is_err()
        );
        terminal.engine.feed(b"\r$ \x1b]133;B\x07");
        assert!(
            !terminal
                .engine
                .automatic_command_ready(terminal.engine.snapshot().automation_revision),
            "a prompt redraw cannot erase editing state"
        );
        // Redrawing the entire prompt also cannot erase an unsubmitted line.
        terminal.engine.feed(b"\r\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(
            !terminal
                .engine
                .automatic_command_ready(terminal.engine.snapshot().automation_revision)
        );
        terminal.engine.input(vec![3])?;
        let _ = input.try_recv()?;
        terminal.engine.feed(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(
            terminal
                .engine
                .automatic_command_ready(terminal.engine.snapshot().automation_revision)
        );
        Ok(())
    }

    #[test]
    fn automatic_commands_require_integrated_empty_input_at_submission() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "automation".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.feed(b"admin@target:~$ ");
        let command = nutty_core::command::automatic_read_only("pwd").expect("read-only command");
        for suffix in ["", "printf PENDING #", "echo $", "printf pending %"] {
            terminal.engine.feed(b"\r\x1b[2Kadmin@target:~$ ");
            terminal.engine.input(suffix.as_bytes().to_vec())?;
            let _ = input.try_recv()?;
            terminal.engine.feed(suffix.as_bytes());
            let revision = terminal.engine.snapshot().automation_revision;
            assert!(
                !terminal.engine.automatic_command_ready(revision),
                "untrusted prompt: {suffix}"
            );
            assert!(
                terminal
                    .engine
                    .send_automatic_command(&command, revision)
                    .is_err()
            );
            assert!(input.try_recv().is_err(), "no appended input");
        }
        terminal.engine.input(vec![3])?;
        let _ = input.try_recv()?;
        terminal.engine.feed(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(terminal.engine.automatic_command_ready(revision));
        terminal.engine.send_automatic_command(&command, revision)?;
        assert!(matches!(input.try_recv()?, Command::Input(_)));
        Ok(())
    }

    #[test]
    fn remote_directory_query_retries_full_queue_and_does_not_leak_into_history() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "remote".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.set_remote_identity("admin", "gateway");
        terminal.engine.feed(b"admin@target:~$ ");
        for _ in 0..64 {
            terminal
                .engine
                .commands
                .try_send(Command::Resize(TermSize::default()))?;
        }
        terminal.engine.request_directory_tracking();
        assert!(!terminal.engine.core.lock().expect("state").awaiting_echo);
        for _ in 0..64 {
            assert!(matches!(input.try_recv()?, Command::Resize(_)));
        }
        terminal.engine.request_directory_tracking();
        let Command::Input(query) = input.try_recv()? else {
            anyhow::bail!("expected directory query");
        };
        assert!(std::str::from_utf8(&query)?.contains("nutty-cwd;"));
        assert!(terminal.engine.command_history().is_empty());
        terminal.engine.request_directory_tracking();
        assert!(input.try_recv().is_err(), "query must not be sent twice");
        Ok(())
    }

    #[test]
    fn remote_bootstrap_never_appends_to_user_input_moved_to_home() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "editing remote".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.feed(b"admin@target:~$ ");
        terminal.engine.input(b"echo pending".to_vec())?;
        input.try_recv()?;
        terminal.engine.feed(b"echo pending\r\x1b[15C");
        terminal.engine.request_directory_tracking();
        assert!(
            input.try_recv().is_err(),
            "cursor at prompt is not an empty input line"
        );
        terminal.engine.input(vec![b'\r'])?;
        input.try_recv()?;
        terminal.engine.feed(b"\r\npending\r\nadmin@target:~$ ");
        terminal.engine.request_directory_tracking();
        assert!(matches!(input.try_recv()?, Command::Input(_)));
        assert!(
            terminal.engine.command_history().is_empty(),
            "unintegrated lines are not guessed"
        );
        Ok(())
    }

    #[test]
    fn bracketed_multiline_paste_and_redraw_remain_pending_until_submission() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "paste".into());
        terminal
            .engine
            .feed(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b[?2004h");
        terminal.engine.paste("printf pending\necho pending")?;
        input.try_recv()?;
        terminal.engine.feed(b"\r\x1b]133;A\x07$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(!terminal.engine.automatic_command_ready(revision));
        assert!(
            terminal
                .engine
                .send_automatic_command("pwd", revision)
                .is_err()
        );
        assert!(input.try_recv().is_err());
        terminal.engine.input(vec![3])?;
        input.try_recv()?;
        terminal.engine.feed(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(
            terminal
                .engine
                .automatic_command_ready(terminal.engine.snapshot().automation_revision)
        );
        Ok(())
    }

    #[test]
    fn typing_ahead_of_the_next_prompt_cannot_be_submitted_by_automation() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "typeahead".into());
        terminal.engine.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        terminal.engine.input(vec![b'\r'])?;
        input.try_recv()?;
        terminal.engine.input(b"printf pending".to_vec())?;
        input.try_recv()?;
        terminal
            .engine
            .feed(b"\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(!terminal.engine.automatic_command_ready(revision));
        assert!(
            terminal
                .engine
                .send_automatic_command("pwd", revision)
                .is_err()
        );
        assert!(
            input.try_recv().is_err(),
            "the new prompt must retain queued typeahead"
        );
        Ok(())
    }

    #[test]
    fn current_directory_osc_is_not_consumed_as_an_inline_image() {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "remote".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.set_remote_identity("admin", "host");
        for chunk in [
            b"\x1b]1337;Cur".as_slice(),
            b"rentDir=/srv/",
            "中文".as_bytes(),
            b"\x07",
        ] {
            terminal.engine.feed(chunk);
        }
        terminal.engine.feed(b"admin@host:~$ ");
        terminal.engine.request_directory_tracking();
        assert_eq!(
            terminal.engine.snapshot().directory.as_deref(),
            Some("/srv/中文")
        );
        assert!(terminal.engine.snapshot().images.is_empty());
        assert!(
            matches!(input.try_recv(), Ok(Command::Input(_))),
            "directory metadata alone still needs command-boundary integration"
        );
    }

    #[test]
    fn current_channel_platform_replaces_gateway_and_requires_a_fresh_prompt() -> Result<()> {
        use crate::session_environment::{Environment, Platform};
        for track_directory in [true, false] {
            let (terminal, input) = Terminal::channel(
                &Preferences {
                    track_directory,
                    ..Default::default()
                },
                "gateway".into(),
            );
            terminal.engine.set_environment(
                Environment::from_ssh_probe(b"NUTTY_ENV_V1\n/bin/zsh\nDarwin\n25.0\narm64\n")
                    .context("gateway facts")?,
            );
            terminal.engine.set_remote_identity("admin", "gateway");
            terminal
                .engine
                .feed(b"\x1b]133;A\x07admin@target:~$ \x1b]133;B\x07");
            let old = terminal.engine.snapshot().automation_revision;
            terminal.engine.request_directory_tracking();
            let Command::Input(query) = input.try_recv()? else {
                anyhow::bail!("query missing")
            };
            assert!(!terminal.engine.automatic_command_ready(old));
            assert_eq!(
                terminal.engine.environment().operating_system,
                Platform::Unknown
            );
            let query = std::str::from_utf8(&query)?;
            let id = query
                .split("nutty-env;")
                .nth(1)
                .context("env token")?
                .split(';')
                .next()
                .context("token")?;
            let marker = query
                .split("nutty-bootstrap;")
                .nth(1)
                .context("echo token")?
                .split('\\')
                .next()
                .context("token")?;
            let response = format!(
                "\x1b]777;nutty-bootstrap;{marker}\x07\x1b]777;nutty-env;{id};NUTTY_ENV_V1\r\n\r\nLinux\r\n6.8.0\r\nx86_64\r\n\x07\x1b]777;nutty-shell;{id};5.2||\x07"
            );
            // Exercise framing across individual bytes, as SSH packets need not align with OSC.
            for byte in response.as_bytes() {
                terminal.engine.feed(std::slice::from_ref(byte));
            }
            let target = terminal.engine.environment();
            assert_eq!(target.operating_system, Platform::Linux);
            assert_eq!(target.active_shell.as_deref(), Some("bash"));
            let revision = terminal.engine.snapshot().automation_revision;
            assert!(
                !terminal.engine.automatic_command_ready(revision),
                "metadata is not an empty prompt"
            );
            terminal
                .engine
                .feed(b"\x1b]133;A\x07\r\x1b[2Kadmin@target:~$ \x1b]133;B\x07");
            let revision = terminal.engine.snapshot().automation_revision;
            assert!(terminal.engine.automatic_command_ready(revision));
            assert!(!terminal.engine.automatic_command_ready(old));
            terminal.engine.feed(b"\r\nadmin@other:~$ ");
            terminal.engine.request_directory_tracking();
            assert_eq!(
                terminal.engine.environment().operating_system,
                Platform::Unknown
            );
            assert!(terminal.engine.command_history().is_empty());
        }
        Ok(())
    }

    #[test]
    fn platform_metadata_is_pane_scoped_and_reconnect_keeps_the_new_environment() {
        use crate::session_environment::{Environment, Platform, Target};
        let preferences = Preferences::default();
        let (local, _) = Terminal::channel(&preferences, "local".into());
        let startup_shell = preferences.shell.clone();
        local
            .engine
            .set_environment(Environment::local(&startup_shell));
        local.engine.feed(b"\x1b]7;file:///Users/example\x07");
        let changed = Preferences {
            shell: "/different/shell".into(),
            ..preferences.clone()
        };
        local.engine.apply_preferences(&changed);
        assert_eq!(
            local.engine.environment().startup_shell.as_deref(),
            Some(startup_shell.to_string_lossy().as_ref()),
            "changing future Shell preferences does not change a running session"
        );
        let (remote, _) = Terminal::channel(&preferences, "remote".into());
        remote.engine.set_environment(
            Environment::from_ssh_probe(b"NUTTY_ENV_V1\n/bin/bash\nLinux\n6.8.0\nx86_64\n")
                .expect("complete remote fixture"),
        );
        remote.engine.feed(b"\x1b]7;file://server/srv/current\x07");
        assert_eq!(local.engine.environment().target, Target::Local);
        assert_eq!(
            local
                .engine
                .environment()
                .last_observed_directory
                .as_deref(),
            Some("/Users/example")
        );
        assert_eq!(
            remote.engine.environment().operating_system,
            Platform::Linux
        );
        assert_eq!(
            remote
                .engine
                .environment()
                .last_observed_directory
                .as_deref(),
            Some("/srv/current")
        );
        local.engine.finish();
        remote.engine.inherit_screen(&local.engine);
        let current = remote.engine.environment();
        assert_eq!(current.target, Target::Ssh);
        assert_eq!(
            current.operating_system,
            Platform::Linux,
            "restored history cannot replace current platform facts"
        );
        assert_eq!(
            current.last_observed_directory.as_deref(),
            Some("/srv/current")
        );
        remote.engine.set_environment(Environment::unknown_ssh());
        assert!(
            remote
                .engine
                .environment()
                .last_observed_directory
                .is_none(),
            "a new transport must not keep an old directory"
        );
    }
    use std::time::Duration;
    #[test]
    fn manual_commands_reach_nested_ssh_prompts_without_relaxing_automation() -> Result<()> {
        let (terminal, wire) = Terminal::channel(&Preferences::default(), "gateway".into());
        terminal.engine.feed(b"gateway$ \x1b]133;B\x07");
        terminal.engine.input(b"ssh target\r".to_vec())?;
        assert!(matches!(wire.recv_blocking()?, Command::Input(_)));
        terminal.engine.feed(b"\r\nadmin@target:~$ \x1b[?2004h");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(!terminal.engine.automatic_command_ready(revision));
        assert!(
            terminal
                .engine
                .send_automatic_command("pwd", revision)
                .is_err()
        );
        assert!(wire.try_recv().is_err());
        terminal.engine.send_command("printf 'one\\ntwo'\npwd")?;
        assert!(matches!(wire.recv_blocking()?, Command::Input(bytes)
            if bytes == b"\x1b[200~printf 'one\\ntwo'\npwd\x1b[201~\r"));
        assert!(terminal.engine.send_command("pwd").is_err());
        terminal.engine.feed(b"\r\nadmin@target:~$ partial");
        assert!(terminal.engine.send_command("pwd").is_err());
        terminal.engine.feed(b"\r\nadmin@target:~$ \x1b[?1049h");
        assert!(terminal.engine.send_command("pwd").is_err());
        assert!(wire.try_recv().is_err());
        Ok(())
    }
    #[test]
    fn sixel_advertises_capability_and_display_mode_keeps_cursor_position() -> Result<()> {
        let (terminal, wire) = Terminal::channel(&Preferences::default(), "sixel".into());
        terminal.engine.feed(b"\x1b[c");
        assert!(matches!(wire.recv_blocking()?, Command::Input(bytes) if bytes == b"\x1b[?62;4c"));
        terminal
            .engine
            .feed(b"\x1b[5;7H\x1b[?80h\x1bP0;1q!4~\x1b\\");
        assert_eq!(terminal.engine.snapshot().cursor, Some((6, 4)));
        terminal.engine.feed(b"\x1b[?80$p");
        assert!(matches!(wire.recv_blocking()?, Command::Input(bytes) if bytes == b"\x1b[?80;1$y"));
        terminal.engine.feed(b"\x1b[?80l\x1bP0;1q!4~\x1b\\");
        assert_eq!(terminal.engine.snapshot().cursor, Some((0, 5)));
        Ok(())
    }
    #[test]
    fn inline_images_decode_off_thread_without_payload_leaks_and_stay_pane_local() -> Result<()> {
        use base64::Engine as _;
        let image = image::RgbaImage::from_pixel(4, 6, image::Rgba([250, 10, 20, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png)?;
        let encoded = base64::prelude::BASE64_STANDARD.encode(png.get_ref());
        let frames = [
            b"\x1bP0;1q\"1;1;4;6#1;2;100;0;0!4~\x1b\\".to_vec(),
            format!("\x1b]1337;File=inline=1;size={};width=4;preserveAspectRatio=1:{}\x07", png.get_ref().len(), encoded).into_bytes(),
            format!("\x1b]1337;MultipartFile=inline=1;size={}\x07\x1b]1337;FilePart={}\x07\x1b]1337;FilePart={}\x07\x1b]1337;FileEnd\x07", png.get_ref().len(), &encoded[..encoded.len()/2], &encoded[encoded.len()/2..]).into_bytes(),
        ];
        for frame in frames {
            let (terminal, _) = Terminal::channel(&Preferences::default(), "inline".into());
            let (other, _) = Terminal::channel(&Preferences::default(), "other".into());
            for bytes in frame.chunks(7) {
                terminal.engine.feed(bytes);
            }
            terminal.engine.feed(b"after-image");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while terminal.engine.snapshot().images.is_empty() {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "inline image decode timeout"
                );
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(terminal.engine.snapshot().images.len(), 1);
            assert!(other.engine.snapshot().images.is_empty());
            assert_eq!(terminal.engine.recent_output().trim(), "after-image");
            terminal.engine.feed(b"\x1b[?1049h");
            assert!(terminal.engine.snapshot().images.is_empty());
            terminal.engine.feed(b"\x1b[?1049l");
            assert_eq!(terminal.engine.snapshot().images.len(), 1);
        }
        Ok(())
    }
    #[test]
    fn cursor_preferences_apply_live_and_program_overrides_reset_to_the_new_defaults() {
        use alacritty_terminal::vte::ansi::CursorShape;
        let mut preferences = Preferences::default();
        let (terminal, _input) = Terminal::channel(&preferences, "cursor".into());
        assert_eq!(
            terminal.engine.snapshot().cursor_style.shape,
            CursorShape::Beam
        );
        preferences.terminal.cursor_shape = crate::config::CursorShape::Underline;
        preferences.terminal.cursor_blink = false;
        terminal.engine.apply_preferences(&preferences);
        assert_eq!(
            terminal.engine.snapshot().cursor_style,
            preferences.terminal.cursor_style()
        );
        terminal.engine.feed(b"\x1b[1 q");
        assert_eq!(
            terminal.engine.snapshot().cursor_style.shape,
            CursorShape::Block
        );
        assert!(terminal.engine.snapshot().cursor_style.blinking);
        preferences.terminal.cursor_shape = crate::config::CursorShape::Beam;
        terminal.engine.apply_preferences(&preferences);
        assert_eq!(
            terminal.engine.snapshot().cursor_style.shape,
            CursorShape::Block,
            "a running TUI keeps its explicit cursor override"
        );
        terminal.engine.feed(b"\x1b[0 q");
        assert_eq!(
            terminal.engine.snapshot().cursor_style,
            preferences.terminal.cursor_style()
        );
    }

    #[test]
    fn images_follow_existing_scroll_region_without_moving_outside_placements() -> Result<()> {
        let (terminal, _) = Terminal::channel(&Preferences::default(), "images".into());
        terminal.engine.feed(b"\x1b[3;6r\x1b[4;1H\x1b_Ga=T,i=21,f=32,s=1,v=1,c=1,r=1,C=1;AQIDBA==\x1b\\\x1b[8;1H\x1b_Ga=T,i=22,f=32,s=1,v=1,c=1,r=1,C=1;AQIDBA==\x1b\\");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while terminal.engine.snapshot().images.len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(terminal.engine.snapshot().images.len(), 2);
        terminal.engine.feed(b"\x1b[6;1H\n");
        let rows: Vec<_> = terminal
            .engine
            .snapshot()
            .images
            .iter()
            .map(|image| image.row)
            .collect();
        assert!(
            rows.contains(&2.),
            "image in the existing scroll region must move"
        );
        assert!(
            rows.contains(&7.),
            "image outside the region must remain in place"
        );
        terminal.engine.feed(b"\x1b[?1049h\x1b[?1049l");
        assert_eq!(
            terminal.engine.snapshot().images.len(),
            2,
            "alternate screen must preserve primary placements"
        );
        Ok(())
    }

    #[test]
    fn kitty_cursor_policy_and_reconnected_terminal_replies_keep_their_own_transport() -> Result<()>
    {
        let (first, _) = Terminal::channel(&Preferences::default(), "first".into());
        first
            .engine
            .feed(b"\x1b[5;3H\x1b_Ga=T,i=7,f=32,s=1,v=1,c=4,r=3;AQIDBA==\x1b\\");
        assert_eq!(
            first.engine.snapshot().cursor,
            Some((6, 7)),
            "image placement moves right and down from the original cursor"
        );
        first
            .engine
            .feed(b"\x1b_Ga=T,i=8,f=32,s=1,v=1,c=4,r=3,C=1;AQIDBA==\x1b\\");
        assert_eq!(
            first.engine.snapshot().cursor,
            Some((6, 7)),
            "C=1 never moves the cursor"
        );
        first.engine.feed(b"\x1b]7;file:///tmp/reconnected\x07");
        first.engine.finish();
        let (second, commands) = Terminal::channel(&Preferences::default(), "replacement".into());
        second.engine.inherit_screen(&first.engine);
        assert_eq!(
            second.engine.snapshot().directory.as_deref(),
            Some("/tmp/reconnected")
        );
        second.engine.feed(b"\x1b[5n");
        let mut response = false;
        while let Ok(command) = commands.try_recv() {
            if let Command::Input(data) = command {
                response |= data == b"\x1b[0n";
            }
        }
        assert!(
            response,
            "inherited Alacritty state must send replies to the replacement transport"
        );
        Ok(())
    }

    #[test]
    fn command_history_follows_the_terminal_across_reconnect_and_clears_on_close() -> Result<()> {
        let (first, _first_commands) = Terminal::channel(&Preferences::default(), "first".into());
        let (other, _) = Terminal::channel(&Preferences::default(), "other".into());
        first
            .engine
            .feed(b"\x1b]7;file:///tmp/first\x07$ \x1b]133;B\x07pwd");
        first.engine.input(vec![b'\r'])?;
        assert_eq!(first.engine.command_history()[0].command, "pwd");
        assert_eq!(
            first.engine.command_history()[0].directory.as_deref(),
            Some("/tmp/first")
        );
        assert!(
            other.engine.command_history().is_empty(),
            "split pane histories must be isolated"
        );
        first.engine.feed(b"\r\n$ \x1b]133;B\x07");
        first.engine.send_command("printf 'one\\ntwo'\npwd")?;
        assert_eq!(first.engine.command_history().len(), 2);
        assert!(first.engine.send_command("not submitted").is_err());
        assert_eq!(
            first.engine.command_history().len(),
            2,
            "rejected submissions never enter history"
        );
        first.engine.finish();
        let (replacement, _) = Terminal::channel(&Preferences::default(), "reconnected".into());
        replacement.engine.inherit_screen(&first.engine);
        assert_eq!(replacement.engine.command_history().len(), 2);
        drop(first);
        assert_eq!(
            replacement.engine.command_history().len(),
            2,
            "old transport disposal cannot erase the surviving pane"
        );
        let engine = replacement.engine.clone();
        drop(replacement);
        assert!(
            engine.command_history().is_empty(),
            "closing the pane clears even retained engine handles"
        );
        assert!(other.engine.command_history().is_empty());
        Ok(())
    }

    #[test]
    fn command_submission_is_atomic_and_rejects_busy_or_fullscreen_input() -> Result<()> {
        let (terminal, receiver) = Terminal::channel(&Preferences::default(), "composer".into());
        terminal.engine.feed(b"$ \x1b]133;B\x07\x1b[?2004h");
        let generation = terminal.engine.snapshot().automation_revision;
        assert!(terminal.engine.automatic_command_ready(generation));
        let command = "cat <<'EOF'\na 'quoted' $VALUE\nEOF";
        terminal.engine.send_command(command)?;
        match receiver.recv_blocking()? {
            Command::Input(bytes) => {
                assert_eq!(bytes, format!("\x1b[200~{command}\x1b[201~\r").as_bytes())
            }
            other => panic!("unexpected command submission: {other:?}"),
        }
        assert!(
            !terminal.engine.automatic_command_ready(generation),
            "queued input invalidates automation immediately"
        );
        assert!(
            terminal.engine.send_command("pwd").is_err(),
            "commands must not append to pending terminal input"
        );
        terminal.engine.feed(b"partial-input");
        assert!(terminal.engine.send_command("pwd").is_err());
        terminal.engine.feed(b"\r\n$ \x1b[?1049h");
        assert!(
            terminal.engine.send_command("pwd").is_err(),
            "a TUI is not a shell command target"
        );
        assert!(
            receiver.try_recv().is_err(),
            "rejected commands must never enter the PTY queue"
        );
        Ok(())
    }

    #[test]
    fn automatic_commands_recheck_terminal_generation_when_queued() -> Result<()> {
        let (terminal, receiver) = Terminal::channel(&Preferences::default(), "automation".into());
        terminal.engine.feed(b"$ \x1b]133;B\x07");
        let generation = terminal.engine.snapshot().automation_revision;
        assert!(terminal.engine.automatic_command_ready(generation));
        terminal.engine.feed(b"\r\n$ \x1b]133;B\x07");
        assert!(
            terminal
                .engine
                .send_automatic_command("pwd", generation)
                .is_err()
        );
        assert!(
            receiver.try_recv().is_err(),
            "stale requests cannot enqueue commands even at a new prompt"
        );
        let generation = terminal.engine.snapshot().automation_revision;
        terminal
            .engine
            .send_automatic_command("'/bin/pwd'", generation)?;
        assert!(matches!(receiver.recv_blocking()?, Command::Input(_)));
        assert!(
            terminal
                .engine
                .send_automatic_command("'/bin/pwd'", generation)
                .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn automatic_pipeline_executes_in_a_real_pty_after_scrolling_and_selection() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let preferences = Preferences {
            directory: directory.path().into(),
            shell: "/bin/sh".into(),
            ..Default::default()
        };
        let terminal = Terminal::local(&preferences)?;
        terminal
            .engine
            .input(b"PS1=\"$(printf '\\033]133;A\\007$ \\033]133;B\\007')\"\r".to_vec())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let revision = loop {
            let revision = terminal.engine.snapshot().automation_revision;
            if terminal.engine.automatic_command_ready(revision) {
                break revision;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "shell did not reach a prompt"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        terminal.engine.scroll(Scroll::Delta(2));
        terminal.engine.select(0, 0, false, SelectionType::Simple);
        let command =
            crate::command::automatic_read_only("pwd | head -n 1").context("read-only pipeline")?;
        terminal.engine.send_automatic_command(&command, revision)?;
        let expected = std::fs::canonicalize(directory.path())?
            .to_string_lossy()
            .into_owned();
        loop {
            if terminal.engine.recent_output().contains(&expected) {
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "automatic command did not execute"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(terminal.engine.command_history().len(), 1);
        Ok(())
    }

    #[test]
    fn viewport_changes_preserve_automation_but_input_and_output_invalidate_it() -> Result<()> {
        let (terminal, receiver) = Terminal::channel(&Preferences::default(), "viewport".into());
        terminal.engine.feed(b"old output\r\n$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        terminal.engine.scroll(Scroll::Delta(3));
        terminal.engine.select(0, 0, false, SelectionType::Simple);
        terminal.engine.resize(TermSize {
            rows: 20,
            columns: 50,
        })?;
        assert!(
            terminal.engine.automatic_command_ready(revision),
            "layout and selection are not shell activity"
        );
        terminal
            .engine
            .send_automatic_command("'/bin/pwd'", revision)?;
        assert!(!terminal.engine.automatic_command_ready(revision));
        assert!(matches!(receiver.recv_blocking()?, Command::Resize(_)));
        assert!(matches!(receiver.recv_blocking()?, Command::Input(_)));
        terminal.engine.feed(b"\r\n$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        terminal.engine.input(b"x".to_vec())?;
        assert!(!terminal.engine.automatic_command_ready(revision));
        Ok(())
    }

    #[test]
    fn clearing_retains_input_row_prompt_and_command_history() -> Result<()> {
        let (terminal, receiver) = Terminal::channel(&Preferences::default(), "clear".into());
        terminal.engine.feed(b"old output\r\n$ \x1b]133;B\x07");
        terminal.engine.send_command("pwd")?;
        let _ = receiver.recv_blocking()?;
        terminal.engine.feed(b"pwd\r\n/tmp\r\n$ \x1b]133;B\x07");
        terminal.engine.select(0, 0, false, SelectionType::Simple);
        terminal.engine.clear_screen()?;
        assert_eq!(terminal.engine.snapshot().cursor, Some((2, 0)));
        assert_eq!(terminal.engine.recent_output(), "$ ".trim_end());
        assert_eq!(terminal.engine.command_history().len(), 1);
        assert!(
            terminal
                .engine
                .automatic_command_ready(terminal.engine.snapshot().automation_revision)
        );
        terminal.engine.send_command("whoami")?;
        assert!(matches!(receiver.recv_blocking()?, Command::Input(_)));
        terminal.engine.feed(b"whoami\r\n$ partial");
        terminal.engine.clear_screen()?;
        assert!(terminal.engine.recent_output().contains("$ partial"));
        assert!(
            terminal.engine.send_command("pwd").is_err(),
            "clearing must not fabricate an empty prompt"
        );
        Ok(())
    }

    #[test]
    fn output_context_is_bounded_unicode_and_independent_of_viewport_or_other_panes() {
        let (first, _) = Terminal::channel(&Preferences::default(), "first".into());
        let (second, _) = Terminal::channel(&Preferences::default(), "second".into());
        for index in 0..160 {
            first
                .engine
                .feed(format!("\x1b[31m中文-{index:03}\x1b[0m\r\n").as_bytes());
        }
        second.engine.feed(b"SECOND_PANE_ONLY");
        let context = first.engine.recent_output();
        assert!(context.contains("中文-159"));
        assert!(
            !context.contains("中文-000"),
            "only recent output belongs in model context"
        );
        assert!(!context.contains("SECOND_PANE_ONLY"));
        assert!(
            !context.contains('\x1b'),
            "VT control sequences must not enter model context"
        );
        assert!(context.len() <= 32 * 1024);
        first.engine.scroll(Scroll::Top);
        assert_eq!(
            first.engine.recent_output(),
            context,
            "scrolling must not change the captured terminal output"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_command_sender_executes_complex_input_in_only_its_own_pty() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let preferences = Preferences {
            shell: if cfg!(windows) {
                "powershell.exe"
            } else {
                "/bin/sh"
            }
            .into(),
            directory: directory.path().to_path_buf(),
            ..Default::default()
        };
        let first = Terminal::local(&preferences)?;
        first
            .engine
            .input(b"PS1=\"$(printf '\\033]133;A\\007$ \\033]133;B\\007')\"\r".to_vec())?;
        let second = Terminal::local(&preferences)?;
        second
            .engine
            .input(b"PS1=\"$(printf '\\033]133;A\\007$ \\033]133;B\\007')\"\r".to_vec())?;
        assert!(
            first
                .engine
                .core
                .lock()
                .expect("terminal state lock poisoned")
                .graphics
                .allow_files
        );
        let (remote, _) = Terminal::channel(&preferences, "SSH".into());
        assert!(
            !remote
                .engine
                .core
                .lock()
                .expect("terminal state lock poisoned")
                .graphics
                .allow_files
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !first
            .engine
            .automatic_command_ready(first.engine.snapshot().automation_revision)
        {
            ensure!(
                std::time::Instant::now() < deadline,
                "local shell did not become ready"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        first.engine.send_command(
            "cat <<'NUTTY_EOF'\ncomplex 'quotes' $HOME\nNUTTY_EOF\nprintf 'NUTTY_SENDER_%s' OK",
        )?;
        loop {
            let snapshot = first.engine.snapshot();
            let text: String = snapshot.cells.iter().map(|cell| cell.cell.c).collect();
            if text.contains("NUTTY_SENDER_OK") {
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "complex command did not execute in its PTY"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !second.engine.recent_output().contains("NUTTY_SENDER_OK"),
            "command sending must never broadcast into another pane"
        );
        Ok(())
    }
    #[test]
    fn ansi_chunks_preserve_unicode_color_and_bracketed_paste() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "test".into());
        terminal.engine.feed(b"\x1b[31mhello\x1b[0m ");
        terminal.engine.feed(&[0xe4, 0xb8]);
        terminal.engine.feed(&[0xad]);
        let snapshot = terminal.engine.snapshot();
        assert_eq!(snapshot.cells[0].cell.c, 'h');
        assert_eq!(
            resolve_color(
                snapshot.cells[0].cell.fg,
                Palette::for_appearance(crate::config::Appearance::Graphite),
                true
            ),
            0xf17c82
        );
        assert_eq!(snapshot.cells[6].cell.c, '中');
        assert!(
            snapshot.cells[7]
                .cell
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
        );
        terminal.engine.feed(b"\x1b[?2004h");
        terminal.engine.paste("echo hi\x1b[201~\n")?;
        match input.recv_blocking()? {
            Command::Input(bytes) => assert_eq!(bytes, b"\x1b[200~echo hi[201~\n\x1b[201~"),
            other => panic!("unexpected command {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn local_pty_executes_and_is_reaped() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let preferences = Preferences {
            directory: directory.path().to_path_buf(),
            shell: if cfg!(windows) {
                "powershell.exe"
            } else {
                "/bin/sh"
            }
            .into(),
            ..Default::default()
        };
        let terminal = Terminal::local(&preferences)?;
        let command = if cfg!(windows) {
            "[Console]::WriteLine('NUTTY_PTY_' + 'OK'); exit\r"
        } else {
            "printf 'NUTTY_PTY_%s' OK; exit\r"
        };
        terminal.engine.input(command.as_bytes().to_vec())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = terminal.engine.snapshot();
            if snapshot.closed {
                let text: String = snapshot.cells.iter().map(|cell| cell.cell.c).collect();
                assert!(
                    text.contains("NUTTY_PTY_OK"),
                    "PTY did not execute printf: {text:?}"
                );
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "PTY did not exit within five seconds"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    fn search_finds_history_wrapped_unicode_and_clears_selection() -> Result<()> {
        let (terminal, _) = Terminal::channel(&Preferences::default(), "search".into());
        terminal.engine.feed(b"historical match\r\n");
        for _ in 0..50 {
            terminal.engine.feed(b"filler\r\n");
        }
        assert!(terminal.engine.search("historical match", false)?);
        assert_eq!(
            terminal.engine.selection().as_deref(),
            Some("historical match")
        );
        assert!(
            terminal
                .engine
                .snapshot()
                .cells
                .iter()
                .any(|cell| cell.selected),
            "search must reveal a match outside the viewport"
        );
        terminal.engine.feed("\r\n中文终端".as_bytes());
        assert!(terminal.engine.search("中文终端", false)?);
        assert_eq!(terminal.engine.selection().as_deref(), Some("中文终端"));
        assert!(!terminal.engine.search("", false)?);
        assert!(terminal.engine.selection().is_none());
        assert!(terminal.engine.search("[", false).is_err());
        Ok(())
    }
}
