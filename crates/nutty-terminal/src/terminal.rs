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
mod context;
mod graphics;
pub use graphics::{DisplayImage, Raster};
mod inline_images;
mod placeholders;
mod protocol;
mod screen;
mod shell;
pub mod zmodem;

/// One MiB accommodates large pastes while keeping each queued message bounded.
const MAX_INPUT: usize = 1024 * 1024;
const CLIPBOARD_LIMIT: usize = 256 * 1024;

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

type ClipboardReply = Arc<dyn Fn(&str) -> String + Send + Sync>;

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
    /// Model context starts at the current target's output, not an earlier host's.
    context: context::SessionStart,
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
    clipboard_copy: Option<String>,
    clipboard_paste: Option<ClipboardReply>,
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
                        osc52: alacritty_terminal::term::Osc52::CopyPaste,
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
                context: Default::default(),
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
                clipboard_copy: None,
                clipboard_paste: None,
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
        // The reaper drops this after the child exits. Windows ConPTY does not
        // signal EOF on the output pipe until the console handle is closed.
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let resize_pty = Arc::clone(&master);
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
                        Command::Resize(size) => resize_pty
                            .lock()
                            .expect("pty master lock poisoned")
                            .as_ref()
                            .map(|pty| pty.resize(size.into()))
                            .unwrap_or(Ok(())),
                    };
                    if let Err(error) = result {
                        engine.fail(format!("终端写入失败：{error}"));
                        break;
                    }
                }
                // The writer actor owns cancellation; the reaper always collects the child.
                let _ = killer.kill();
                let pty = resize_pty.lock().expect("pty master lock poisoned").take();
                drop(pty);
            })?;
        thread::Builder::new()
            .name("nutty-pty-reap".into())
            .spawn(move || {
                let _ = child.wait();
                let pty = master.lock().expect("pty master lock poisoned").take();
                drop(pty);
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
        if environment.target == crate::session_environment::Target::Local {
            // Looked up here so that identifying a target while parsing output stays cheap.
            local_host_label();
        }
        self.flush_bootstrap_echo();
        let mut core = self.core.lock().expect("terminal state lock poisoned");
        // A reconnect from a jump target lands on the gateway, not on that target.
        if core
            .shell
            .target
            .as_ref()
            .is_some_and(|target| !target.transport_bound)
        {
            // Marked after the `[reconnected]` notice that follows.
            core.context.defer();
        }
        core.environment = environment;
        core.shell.reset_location();
        core.shell.bootstrap_input_pending = false;
        core.shell.input_pending = false;
    }

    /// Marks shell hooks as queued for a new SSH shell before they are typed, so the
    /// prompt probe does not send its own bootstrap in the meantime.
    pub fn remote_integration_deferred(&self) {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .shell
            .integration_deferred();
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
        let command = core.shell.probe_command(&prompt);
        mark_target_switch(&mut core, previous_target.as_ref().map(|target| target.id));
        if let Some(command) = command {
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
        current_environment(&self.core.lock().expect("terminal state lock poisoned"))
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
        let from_jump_target = old
            .shell
            .target
            .as_ref()
            .is_some_and(|target| !target.transport_bound);
        std::mem::swap(&mut new.term, &mut old.term);
        std::mem::swap(&mut new.events, &mut old.events);
        std::mem::swap(&mut new.parser, &mut old.parser);
        std::mem::swap(&mut new.graphics, &mut old.graphics);
        std::mem::swap(&mut new.context, &mut old.context);
        new.graphics.allow_files = false;
        new.clipboard_copy = None;
        new.clipboard_paste = None;
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
        // The previous connection's full-screen program is gone, as in `resume`.
        self.feed(b"\x1b[?1049l\r\n\x1b[0m[reconnected]\r\n");
        if from_jump_target {
            let mut core = self.core.lock().expect("terminal state lock poisoned");
            let Core { term, context, .. } = &mut *core;
            context.mark(term);
        }
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
                        context,
                        ..
                    } = &mut *core;
                    parser.advance(
                        &mut screen::Screen {
                            term,
                            graphics,
                            context,
                        },
                        data,
                    );
                }
                protocol::Part::Osc(data) => {
                    let cursor = core.term.grid().cursor.point;
                    let target = target_id(&core);
                    core.shell.osc(data, cursor);
                    mark_target_switch(&mut core, target);
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
                    // An escape inside the string ends it and starts a sequence of its own.
                    let Core {
                        term,
                        parser,
                        graphics,
                        context,
                        ..
                    } = &mut *core;
                    let mut screen = screen::Screen {
                        term,
                        graphics,
                        context,
                    };
                    parser.advance(&mut screen, b"\x1b]");
                    parser.advance(&mut screen, data);
                    parser.advance(&mut screen, b"\x07");
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
                            let Core {
                                term,
                                graphics,
                                context,
                                ..
                            } = &mut *core;
                            let mut screen = screen::Screen {
                                term,
                                graphics,
                                context,
                            };
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
                        let Core {
                            term,
                            parser,
                            graphics,
                            context,
                            ..
                        } = &mut *core;
                        let mut screen = screen::Screen {
                            term,
                            graphics,
                            context,
                        };
                        parser.advance(&mut screen, b"\x1bP");
                        parser.advance(&mut screen, data);
                        parser.advance(&mut screen, b"\x1b\\");
                    }
                }
            });
            core.decoder = decoder;
            let Core { term, context, .. } = &mut *core;
            context.settle(term);
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
                Event::ClipboardStore(kind, text) => {
                    if kind == alacritty_terminal::term::ClipboardType::Clipboard
                        && text.len() <= CLIPBOARD_LIMIT
                        && !text.contains('\0')
                    {
                        self.core
                            .lock()
                            .expect("terminal state lock poisoned")
                            .clipboard_copy = Some(text);
                    }
                }
                Event::ClipboardLoad(kind, format) => {
                    let mut core = self.core.lock().expect("terminal state lock poisoned");
                    let prompt = kind == alacritty_terminal::term::ClipboardType::Clipboard
                        && core.clipboard_paste.is_none();
                    if prompt {
                        core.clipboard_paste = Some(format);
                    } else {
                        let sequence = format("");
                        drop(core);
                        let _ = self
                            .commands
                            .try_send(Command::Input(sequence.into_bytes()));
                    }
                }
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
        let mut cells = Vec::new();
        let mut tiles = Vec::new();
        let mut placeholders = placeholders::Row::default();
        let mut previous_row = None;
        let placeholders_active = core.graphics.has_placeholder();
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let selected = content
                .selection
                .as_ref()
                .is_some_and(|selection| selection.contains(indexed.point));
            let mut cell = indexed.cell.clone();
            if placeholders_active {
                let marks = indexed.cell.zerowidth().unwrap_or(&[]);
                let changed = previous_row != Some(row);
                previous_row = Some(row);
                if let Some(located) = placeholders.observe(
                    indexed.point.column.0,
                    changed,
                    indexed.cell.c,
                    marks,
                    color_number(indexed.cell.fg),
                    indexed
                        .cell
                        .underline_color()
                        .and_then(color_number)
                        .unwrap_or(0),
                ) && let Some(tile) = core.graphics.placeholder_tile(
                    located.image,
                    located.placement,
                    located.image_column,
                    located.image_row,
                    indexed.point.column.0,
                    row as usize,
                ) {
                    tiles.push(tile);
                    cell.clear_wide();
                }
            }
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
            cells.push(DisplayCell {
                column: indexed.point.column.0,
                row: row as usize,
                cell,
                selected,
            });
        }
        let cursor_row = content.cursor.point.line.0 + offset;
        let cursor = (content.mode.contains(TermMode::SHOW_CURSOR)
            && cursor_row >= 0
            && (cursor_row as usize) < rows)
            .then_some((content.cursor.point.column.0, cursor_row.max(0) as usize));
        Snapshot {
            transfer: core.zmodem.status(),
            cells,
            images: {
                let mut images = core.graphics.snapshot(
                    content.display_offset,
                    content.mode.contains(TermMode::ALT_SCREEN),
                );
                images.extend(merge_placeholder_runs(tiles));
                images
            },
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

    pub fn take_clipboard_copy(&self) -> Option<String> {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .clipboard_copy
            .take()
    }

    pub fn clipboard_paste_pending(&self) -> bool {
        self.core
            .lock()
            .expect("terminal state lock poisoned")
            .clipboard_paste
            .is_some()
    }

    pub fn answer_clipboard_paste(&self, text: Option<&str>) {
        let sequence = {
            let mut core = self.core.lock().expect("terminal state lock poisoned");
            let Some(format) = core.clipboard_paste.take() else {
                return;
            };
            let text = text.unwrap_or("");
            let end = text.len().min(CLIPBOARD_LIMIT);
            let end = (0..=end)
                .rev()
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(0);
            format(&text[..end])
        };
        if sequence.len() <= MAX_INPUT {
            let _ = self
                .commands
                .try_send(Command::Input(sequence.into_bytes()));
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
        let columns = core.term.columns();
        let logical = core.context.before_resize(&core.term);
        core.term.resize(size);
        let Core { term, context, .. } = &mut *core;
        context.after_resize(term, logical, term.columns() != columns);
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
        if enter {
            core.shell.command_submitted();
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
        } else if revision.is_some() && command.contains(['\n', '\t']) {
            // Without bracketed paste each line is typed on its own: a program started by
            // an earlier line could read the later lines as its input, and a tab asks the
            // line editor for completion. Nobody watches an automatic send, so the lines
            // go as one brace group, which the shell reads to its end before running any
            // of it.
            ensure!(
                !command.contains('\t')
                    && !command.trim_end().ends_with('\\')
                    && reads_brace_groups(&current_environment(&core)),
                "此终端未启用括号粘贴，无法自动发送多行或含制表符的命令，请点击执行"
            );
            format!("{{\r{}\r}}\r", command.replace('\n', "\r")).into_bytes()
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

    /// Reads recent parsed output of the current target session, independent of the
    /// user's scroll position. Output from a host shown earlier in this pane is excluded.
    pub fn recent_output(&self) -> String {
        const LIMIT: usize = 32 * 1024;
        let core = self.core.lock().expect("terminal state lock poisoned");
        let bottom = core.term.screen_lines() as i32 - 1;
        let session = (!core.term.mode().contains(TermMode::ALT_SCREEN))
            .then(|| core.context.row(&core.term))
            .flatten();
        let top = (bottom - (context::CONTEXT_ROWS - 1))
            .max(core.term.topmost_line().0)
            .max(session.unwrap_or(i32::MIN));
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
        core.context.clear();
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

fn color_number(color: Color) -> Option<u32> {
    match color {
        Color::Indexed(index) => Some(u32::from(index)),
        Color::Spec(color) => {
            Some((u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b))
        }
        Color::Named(_) => None,
    }
}

fn merge_placeholder_runs(mut tiles: Vec<graphics::DisplayImage>) -> Vec<graphics::DisplayImage> {
    tiles.sort_by(|left, right| {
        left.row
            .total_cmp(&right.row)
            .then(left.column.total_cmp(&right.column))
    });
    let mut merged: Vec<graphics::DisplayImage> = Vec::new();
    for tile in tiles {
        if let Some(last) = merged.last_mut()
            && last.image.id == tile.image.id
            && (last.row - tile.row).abs() < 0.01
            && (last.column + last.columns - tile.column).abs() < 0.01
            && (last.crop[1] - tile.crop[1]).abs() < 0.01
            && (last.crop[3] - tile.crop[3]).abs() < 0.01
            && (last.crop[0] + last.crop[2] - tile.crop[0]).abs() < 0.01
        {
            last.columns += tile.columns;
            last.crop[2] += tile.crop[2];
            continue;
        }
        merged.push(tile);
    }
    merged
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
        context: &mut core.context,
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

fn target_id(core: &Core) -> Option<uuid::Uuid> {
    core.shell.target.as_ref().map(|target| target.id)
}

/// A different user@host than before starts a new session for model context; the first
/// identification of a pane's target does not, unless a local pane first identifies another
/// machine. A seeded SSH alias or IP replaced by the host's own name also counts, which only
/// hides that host's login banner.
fn mark_target_switch(core: &mut Core, previous: Option<uuid::Uuid>) {
    let current = target_id(core);
    let switched = if previous.is_some() {
        current.is_some() && current != previous
    } else {
        core.environment.target == crate::session_environment::Target::Local
            && core
                .shell
                .target
                .as_ref()
                .is_some_and(|target| !is_local_machine(&target.name))
    };
    if switched {
        let Core { term, context, .. } = core;
        context.mark(term);
    }
}

/// Whether `user@host` names this machine. Prompts and titles usually show the host up to
/// its first dot, so only that label is compared; any user counts, as `su` or `sudo -s` stays
/// on the same host.
fn is_local_machine(target: &str) -> bool {
    let host = shell::host_label(target);
    host == "localhost" || local_host_label().is_some_and(|local| *local == host)
}

/// First label of this machine's hostname, looked up once per process.
fn local_host_label() -> Option<&'static String> {
    static LABEL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    LABEL
        .get_or_init(|| {
            #[cfg(unix)]
            let host = nix::sys::utsname::uname()
                .ok()
                .and_then(|system| system.nodename().to_str().map(str::to_owned));
            // COMPUTERNAME is the NetBIOS name, upper-cased and cut to 15 characters.
            #[cfg(windows)]
            let host = windows_dns_hostname().or_else(|| std::env::var("COMPUTERNAME").ok());
            #[cfg(not(any(unix, windows)))]
            let host = None::<String>;
            let label = shell::host_label(&host?);
            (!label.is_empty()).then_some(label)
        })
        .as_ref()
}

#[cfg(windows)]
fn windows_dns_hostname() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    let mut size = 0u32;
    // SAFETY: A null buffer with size 0 only asks for the required size.
    unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: `buffer` holds `size` UTF-16 units and lives through the call.
    let read = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            buffer.as_mut_ptr(),
            &mut size,
        )
    };
    if read == 0 {
        return None;
    }
    buffer.truncate(size as usize);
    String::from_utf16(&buffer).ok()
}

fn record_command(core: &mut Core, command: &str) {
    core.history
        .record(command, core.shell.directory.as_deref());
    if let Some(journal) = &core.journal {
        journal.command(command, core.shell.directory.as_deref());
    }
}

fn current_environment(core: &Core) -> crate::session_environment::Environment {
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

/// Shells known to read a `{ … }` group completely before running it. An unidentified
/// shell may be csh or another family where the braces mean something else.
fn reads_brace_groups(environment: &crate::session_environment::Environment) -> bool {
    ["bash", "sh", "zsh", "dash", "ash", "ksh", "mksh"]
        .iter()
        .any(|shell| environment.shell_is(shell))
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
    fn osc52_copies_text_and_paste_waits_for_an_explicit_answer() -> Result<()> {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "clipboard".into());
        terminal.engine.feed(b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(
            terminal.engine.take_clipboard_copy().as_deref(),
            Some("hello")
        );
        terminal.engine.feed(b"\x1b]52;c;?\x07");
        assert!(terminal.engine.clipboard_paste_pending());
        terminal.engine.answer_clipboard_paste(None);
        let response = input.try_recv()?;
        let Command::Input(bytes) = response else {
            panic!("paste denial must answer the terminal");
        };
        let text = String::from_utf8(bytes)?;
        assert!(text.starts_with("\u{1b}]52;c;"));
        assert!(!text.contains("hello"));
        assert!(!terminal.engine.clipboard_paste_pending());
        Ok(())
    }

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
        let command = "pwd";
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
                    .send_automatic_command(command, revision)
                    .is_err()
            );
            assert!(input.try_recv().is_err(), "no appended input");
        }
        terminal.engine.input(vec![3])?;
        let _ = input.try_recv()?;
        terminal.engine.feed(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
        let revision = terminal.engine.snapshot().automation_revision;
        assert!(terminal.engine.automatic_command_ready(revision));
        terminal.engine.send_automatic_command(command, revision)?;
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

    #[test]
    fn automatic_multiline_commands_without_bracketed_paste_are_sent_as_one_group() -> Result<()> {
        let shell = |shell: Option<&str>| crate::session_environment::Environment {
            target: crate::session_environment::Target::Local,
            operating_system: crate::session_environment::Platform::Linux,
            startup_shell: shell.map(Into::into),
            ..Default::default()
        };
        let (terminal, receiver) = Terminal::channel(&Preferences::default(), "group".into());
        let sent = |command: &str, automatic: bool| -> Result<Option<Vec<u8>>> {
            terminal
                .engine
                .feed(b"\r\n\x1b]133;D\x07\x1b]133;A\x07$ \x1b]133;B\x07");
            let revision = terminal.engine.snapshot().automation_revision;
            ensure!(terminal.engine.automatic_command_ready(revision));
            let result = if automatic {
                terminal.engine.send_automatic_command(command, revision)
            } else {
                terminal.engine.send_command(command)
            };
            Ok(match result {
                Ok(()) => match receiver.try_recv()? {
                    Command::Input(bytes) => Some(bytes),
                    other => anyhow::bail!("unexpected submission: {other:?}"),
                },
                Err(_) => {
                    ensure!(receiver.try_recv().is_err(), "a refusal sends nothing");
                    None
                }
            })
        };
        let script = "uname -a\nless /etc/hosts\nq";
        terminal.engine.set_environment(shell(Some("/bin/bash")));
        assert_eq!(
            sent(script, true)?.as_deref(),
            Some(b"{\runame -a\rless /etc/hosts\rq\r}\r".as_slice()),
            "a pager started by one line must not read the next line as keystrokes"
        );
        assert_eq!(
            terminal
                .engine
                .command_history()
                .last()
                .map(|entry| entry.command.clone()),
            Some(script.to_string()),
            "history keeps the command as proposed"
        );
        assert_eq!(
            sent("pwd", true)?.as_deref(),
            Some(b"pwd\r".as_slice()),
            "one line needs no group"
        );
        assert_eq!(
            sent(script, false)?.as_deref(),
            Some(b"uname -a\rless /etc/hosts\rq\r".as_slice()),
            "a clicked command is typed as before"
        );
        assert_eq!(
            sent("printf 'a\tb'", true)?,
            None,
            "a tab would ask for completion"
        );
        assert_eq!(
            sent("ls \\\n-la \\", true)?,
            None,
            "a trailing backslash would swallow the brace"
        );
        for other in [Some("/usr/bin/fish"), Some("/bin/tcsh"), Some("pwsh"), None] {
            terminal.engine.set_environment(shell(other));
            assert_eq!(
                sent(script, true)?,
                None,
                "{other:?} is not known to group with braces"
            );
            assert!(
                sent("pwd", true)?.is_some(),
                "{other:?} still takes one line"
            );
        }
        terminal
            .engine
            .set_environment(shell(Some("/usr/bin/fish")));
        terminal.engine.feed(b"\x1b[?2004h");
        assert_eq!(
            sent(script, true)?.as_deref(),
            Some(format!("\x1b[200~{script}\x1b[201~\r").as_bytes()),
            "bracketed paste delivers every line as one submission"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn automatic_multiline_commands_run_completely_in_a_real_pty() -> Result<()> {
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
        // `cat` would read the following lines as its input if they were typed one by one.
        terminal.engine.send_automatic_command(
            "cat <<'EOF' # first\nheredoc $HOME line\nEOF\ncat </dev/null\nprintf '%s-%s\\n' group done",
            revision,
        )?;
        loop {
            let output = terminal.engine.recent_output();
            if output.lines().any(|line| line == "group-done") {
                ensure!(
                    output.lines().any(|line| line == "heredoc $HOME line"),
                    "here-document was not delivered intact: {output}"
                );
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "automatic group did not finish: {output}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
        terminal
            .engine
            .send_automatic_command("pwd | head -n 1", revision)?;
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
    fn output_context_starts_at_the_current_target_after_scroll_clear_and_reflow() -> Result<()> {
        let (terminal, _input) = Terminal::channel(&Preferences::default(), "gateway".into());
        terminal.engine.feed(b"local-before-title\r\n");
        terminal.engine.feed(b"\x1b]0;admin@alpha: ~\x07");
        assert!(
            terminal
                .engine
                .recent_output()
                .contains("local-before-title"),
            "the first identification of a remote pane's target is not a switch"
        );
        let long = "alpha-wrapped-".repeat(20);
        terminal
            .engine
            .feed(format!("alpha-secret-1\r\n{long}\r\nadmin@alpha:~$ exit\r\n").as_bytes());
        terminal
            .engine
            .feed(b"\x1b]0;admin@beta: ~\x07admin@beta:~$ uptime\r\nbeta-load\r\n");
        let context = terminal.engine.recent_output();
        assert!(context.contains("admin@beta:~$ uptime") && context.contains("beta-load"));
        assert!(
            !context.contains("alpha-secret-1") && !context.contains("alpha-wrapped"),
            "{context}"
        );
        for index in 0..45 {
            terminal
                .engine
                .feed(format!("beta-{index:02}\r\n").as_bytes());
        }
        let context = terminal.engine.recent_output();
        assert!(context.contains("beta-load") && context.contains("beta-44"));
        assert!(
            !context.contains("alpha"),
            "start follows rows into scrollback"
        );
        terminal.engine.resize(TermSize {
            columns: 30,
            rows: 12,
        })?;
        let context = terminal.engine.recent_output();
        assert!(context.contains("beta-load") && context.contains("beta-44"));
        assert!(
            !context.contains("alpha"),
            "start follows reflow: {context}"
        );
        terminal.engine.resize(TermSize::default())?;
        assert!(!terminal.engine.recent_output().contains("alpha"));
        terminal.engine.feed(b"\x1b[H\x1b[2J");
        let context = terminal.engine.recent_output();
        assert!(context.contains("beta-44") && !context.contains("alpha"));
        terminal.engine.feed(b"\x1b[3Jbeta-after-clear\r\n");
        let context = terminal.engine.recent_output();
        assert!(context.contains("beta-after-clear") && !context.contains("alpha"));
        // Returning to the first host is another switch.
        terminal
            .engine
            .feed(b"\x1b]0;admin@alpha: ~\x07alpha-again\r\n");
        let context = terminal.engine.recent_output();
        assert!(context.contains("alpha-again") && !context.contains("beta"));
        terminal.engine.feed(b"\x1bc");
        terminal.engine.feed(b"after-reset\r\n");
        assert!(terminal.engine.recent_output().contains("after-reset"));
        Ok(())
    }

    #[test]
    fn alternate_screen_cursor_jumps_and_reconnects_keep_the_session_start() -> Result<()> {
        let switched = || {
            let (terminal, input) = Terminal::channel(&Preferences::default(), "gw".into());
            terminal
                .engine
                .feed(b"\x1b]0;admin@alpha: ~\x07alpha-secret\r\n");
            terminal
                .engine
                .feed(b"\x1b]0;admin@beta: ~\x07beta-start\r\n");
            for index in 0..60 {
                terminal
                    .engine
                    .feed(format!("beta-{index:02}\r\n").as_bytes());
            }
            (terminal, input)
        };
        let (terminal, _input) = switched();
        terminal.engine.feed(b"\x1b[?1049hvim");
        terminal.engine.resize(TermSize {
            columns: 60,
            rows: 40,
        })?;
        terminal.engine.feed(b"\x1b[3J\x1b[?1049l");
        let context = terminal.engine.recent_output();
        assert!(context.contains("beta-59"));
        assert!(
            !context.contains("alpha-secret"),
            "full-screen programs cannot reset the primary session start"
        );
        let (terminal, _input) = switched();
        terminal.engine.feed(b"\x1b[H");
        terminal.engine.resize(TermSize {
            columns: 40,
            rows: 20,
        })?;
        assert!(
            !terminal.engine.recent_output().contains("alpha-secret"),
            "a cursor above the start cannot pull the start upwards"
        );
        // A switch observed inside the alternate screen applies once it closes.
        let (terminal, _input) = switched();
        terminal
            .engine
            .feed(b"\x1b[?1049h\x1b]0;admin@gamma: ~\x07\x1b[?1049lgamma-start\r\n");
        let context = terminal.engine.recent_output();
        assert!(context.contains("gamma-start") && !context.contains("beta-59"));
        // Reconnecting a pane that showed a jump target lands on the gateway.
        let (terminal, _input) = Terminal::channel(&Preferences::default(), "gw".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.set_remote_identity("ops", "bastion");
        terminal
            .engine
            .feed(b"\x1b]0;admin@web01: ~\x07web01-secret\r\n");
        terminal.engine.pause();
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.set_remote_identity("ops", "bastion");
        terminal.engine.resume();
        terminal.engine.feed(b"bastion-new\r\n");
        let context = terminal.engine.recent_output();
        assert!(context.contains("bastion-new"));
        assert!(!context.contains("web01-secret"), "{context}");
        Ok(())
    }

    #[test]
    fn prompt_probe_on_a_new_jump_target_starts_a_new_context() {
        let (terminal, input) = Terminal::channel(&Preferences::default(), "gateway".into());
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::unknown_ssh());
        terminal.engine.set_remote_identity("ops", "bastion");
        terminal
            .engine
            .feed(b"bastion-menu-secret\r\nOpt> web01\r\nLast login: today\r\nadmin@web01 ~ % ");
        terminal.engine.request_directory_tracking();
        assert!(matches!(input.try_recv(), Ok(Command::Input(_))));
        let context = terminal.engine.recent_output();
        assert!(context.starts_with("admin@web01 ~ %"), "{context}");
        assert!(!context.contains("bastion-menu-secret"));
    }

    /// A pane that showed `admin@alpha`, then switched to `admin@beta` at `BETASTART`.
    fn alpha_then_beta(
        preferences: &Preferences,
        size: TermSize,
        alpha: &[usize],
    ) -> Result<(Terminal, Receiver<Command>)> {
        let (terminal, input) = Terminal::channel(preferences, "gw".into());
        terminal.engine.resize(size)?;
        terminal.engine.feed(b"\x1b]0;admin@alpha: ~\x07");
        for (index, length) in alpha.iter().enumerate() {
            let line = format!("ALPHA{index}{}\r\n", "x".repeat(*length));
            terminal.engine.feed(line.as_bytes());
        }
        terminal
            .engine
            .feed(b"ALPHA-tail\r\n\x1b]0;admin@beta: ~\x07BETASTART\r\n");
        Ok((terminal, input))
    }

    #[test]
    fn local_pane_output_is_excluded_once_the_pane_identifies_another_machine() {
        let local = |title: &str| {
            let (terminal, input) = Terminal::channel(&Preferences::default(), "zsh".into());
            terminal
                .engine
                .set_environment(crate::session_environment::Environment::local(
                    std::path::Path::new("/bin/zsh"),
                ));
            terminal.engine.feed(
                b"me@mac ~ % cat secrets.txt\r\nLOCAL-SECRET\r\nme@mac ~ % ssh admin@web01\r\n",
            );
            terminal
                .engine
                .feed(format!("\x1b]0;{title}\x07{title}$ ").as_bytes());
            (terminal, input)
        };
        let host = local_host_label()
            .cloned()
            .unwrap_or_else(|| "localhost".into());
        for own in [
            format!("me@{host}: ~"),
            format!("root@{}.local: /etc", host.to_uppercase()),
            "me@localhost: ~".into(),
        ] {
            let (terminal, _input) = local(&own);
            let context = terminal.engine.recent_output();
            assert!(context.contains("LOCAL-SECRET"), "{own}: {context}");
        }
        let (terminal, _input) = local("admin@nutty-remote-fixture: ~");
        let context = terminal.engine.recent_output();
        assert!(!context.contains("LOCAL-SECRET"), "{context}");
        assert!(
            context.starts_with("admin@nutty-remote-fixture: ~$"),
            "{context}"
        );
        // Back on the local shell, whose hooks report its full hostname, and out again.
        terminal.engine.feed(
            format!("exit\r\n\x1b]7;file://{host}.local/Users/me\x07me@mac ~ % cat notes\r\nLOCAL-AGAIN\r\n")
                .as_bytes(),
        );
        let context = terminal.engine.recent_output();
        assert!(context.starts_with("me@mac ~ % cat notes"), "{context}");
        terminal
            .engine
            .input(b"ssh admin@nutty-remote-fixture\r".to_vec())
            .expect("submit ssh");
        terminal
            .engine
            .feed(b"\x1b]0;admin@nutty-remote-fixture: ~\x07remote-again\r\n");
        assert_eq!(terminal.engine.recent_output(), "remote-again");
    }

    #[test]
    fn a_local_shell_whose_title_names_another_host_keeps_one_session() {
        let (terminal, _input) = Terminal::channel(
            &Preferences {
                track_directory: true,
                ..Default::default()
            },
            "zsh".into(),
        );
        terminal
            .engine
            .set_environment(crate::session_environment::Environment::local(
                std::path::Path::new("/bin/zsh"),
            ));
        let host = local_host_label()
            .cloned()
            .unwrap_or_else(|| "localhost".into());
        let mut ids = std::collections::HashSet::new();
        for n in 0..4 {
            terminal.engine.feed(
                format!(
                    "\x1b]133;D;0\x07\x1b]7;file://{host}.local/Users/me\x07\x1b]133;A\x07\
                     \x1b]0;me@devbox: ~\x07me@devbox:~$ \x1b]133;B\x07ls\r\n\
                     \x1b]133;C\x07OUT{n}\r\n"
                )
                .as_bytes(),
            );
            ids.extend(terminal.engine.directory_target().map(|target| target.id));
        }
        assert!(ids.len() <= 2, "the target changed {} times", ids.len());
        let context = terminal.engine.recent_output();
        assert!(
            context.contains("OUT1") && context.contains("OUT3"),
            "{context}"
        );
    }

    #[test]
    fn a_host_whose_title_and_osc7_names_differ_keeps_one_session() {
        let (terminal, _input) = Terminal::channel(
            &Preferences {
                track_directory: true,
                ..Default::default()
            },
            "gw".into(),
        );
        let mut ids = std::collections::HashSet::new();
        for n in 0..4 {
            terminal.engine.feed(
                format!(
                    "\x1b]133;D;0\x07\x1b]7;file://db-01.internal/home/admin\x07\x1b]133;A\x07\
                     \x1b]0;admin@prod-db: ~\x07admin@prod-db:~$ ls\r\nOUT{n}\r\n"
                )
                .as_bytes(),
            );
            ids.extend(terminal.engine.directory_target().map(|target| target.id));
        }
        assert_eq!(ids.len(), 2, "named by the title, then once by OSC 7");
        let context = terminal.engine.recent_output();
        assert!(
            context.contains("OUT1") && context.contains("OUT3"),
            "{context}"
        );
    }

    #[test]
    fn manual_reconnect_leaves_the_previous_connections_alternate_screen() {
        let bound = || {
            let mut environment = crate::session_environment::Environment::unknown_ssh();
            environment.startup_shell = Some("/bin/bash".into());
            environment
        };
        let (old, _old_input) = Terminal::channel(&Preferences::default(), "gw".into());
        old.engine.set_environment(bound());
        old.engine.set_remote_identity("ops", "bastion");
        old.engine
            .feed(b"\x1b]0;admin@web01: ~\x07web01-secret\r\n\x1b[?1049hweb01-top-screen");
        old.engine.finish();
        let (new, _new_input) = Terminal::channel(&Preferences::default(), "gw".into());
        new.engine.set_environment(bound());
        new.engine.set_remote_identity("ops", "bastion");
        new.engine.inherit_screen(&old.engine);
        new.engine.feed(b"bastion-new\r\n");
        assert!(!new.engine.snapshot().mode.contains(TermMode::ALT_SCREEN));
        let context = new.engine.recent_output();
        assert!(context.contains("bastion-new"), "{context}");
        assert!(!context.contains("web01"), "{context}");
    }

    #[test]
    fn line_insertion_and_deletion_outside_the_scroll_region_keep_the_session_start() -> Result<()>
    {
        for sequence in [
            "\x1b[10;20r\x1b[H\x1b[5M\x1b[r",
            "\x1b[10;20r\x1b[4;1H\x1b[8M\x1b[r",
            "\x1b[10;20r\x1b[H\x1b[3L\x1b[r",
        ] {
            let (terminal, _input) =
                alpha_then_beta(&Preferences::default(), TermSize::default(), &[0; 5])?;
            terminal.engine.feed(b"beta-1\r\nbeta-2\r\n");
            terminal.engine.feed(sequence.as_bytes());
            let context = terminal.engine.recent_output();
            assert!(
                context.starts_with("BETASTART\nbeta-1"),
                "{sequence:?}: {context}"
            );
        }
        Ok(())
    }

    #[test]
    fn images_outside_the_scroll_region_ignore_line_deletion_above_it() -> Result<()> {
        let (terminal, _) = Terminal::channel(&Preferences::default(), "images".into());
        terminal.engine.feed(
            b"\x1b[5;1H\x1b_Ga=T,i=31,f=32,s=1,v=1,c=1,r=1,C=1;AQIDBA==\x1b\\\x1b[10;20r\x1b[2;1H\x1b[2M\x1b[r",
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while terminal.engine.snapshot().images.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows: Vec<_> = terminal
            .engine
            .snapshot()
            .images
            .iter()
            .map(|image| image.row)
            .collect();
        assert_eq!(rows, [4.]);
        Ok(())
    }

    #[test]
    fn shrinking_below_a_cursor_above_the_session_start_fails_closed() -> Result<()> {
        let preferences = Preferences {
            scrollback: 30,
            ..Default::default()
        };
        let size = TermSize {
            columns: 24,
            rows: 30,
        };
        let (terminal, _input) = alpha_then_beta(&preferences, size, &[31, 140, 8])?;
        terminal.engine.feed(b"beta-old\r\n\x1b[11;1H");
        terminal.engine.resize(TermSize {
            columns: 112,
            rows: 3,
        })?;
        assert_eq!(terminal.engine.recent_output(), "");
        // The rows kept at the bottom predate the session; output that follows belongs to it,
        // also across later resizes.
        terminal.engine.feed(b"overwrite\r\nbeta-new\r\n");
        terminal.engine.resize(TermSize::default())?;
        terminal.engine.feed(b"\x1b[?1049h\x1b[?1049l");
        assert_eq!(terminal.engine.recent_output(), "beta-new");
        Ok(())
    }

    #[test]
    fn reflow_joining_the_cursor_line_cannot_raise_the_session_start() -> Result<()> {
        // The cursor row's wrap flag goes stale when the rows below it are dropped, and a
        // later narrowing joins its line with the next.
        let preferences = Preferences {
            scrollback: 30,
            ..Default::default()
        };
        let size = TermSize {
            columns: 47,
            rows: 16,
        };
        let (terminal, _input) = alpha_then_beta(&preferences, size, &[127])?;
        terminal
            .engine
            .feed(format!("b{}\r\nb{}\r\n\x1b[10;1H", "z".repeat(156), "z".repeat(84)).as_bytes());
        terminal.engine.resize(TermSize {
            columns: 69,
            rows: 10,
        })?;
        terminal
            .engine
            .feed(format!("b{}\r\n\x1b[?1049h", "z".repeat(45)).as_bytes());
        terminal.engine.resize(TermSize {
            columns: 46,
            rows: 8,
        })?;
        terminal.engine.feed(b"\x1b[?1049l");
        let context = terminal.engine.recent_output();
        assert!(context.starts_with("BETASTART\n"), "{context}");
        Ok(())
    }

    #[test]
    fn reflow_joining_a_line_shortened_by_character_deletion_cannot_raise_the_session_start()
    -> Result<()> {
        // Deleting characters moves the wrap flag off the last column; a narrower width then
        // continues that line into the next one, where gamma's session starts.
        let size = TermSize {
            columns: 20,
            rows: 10,
        };
        let (terminal, _input) = alpha_then_beta(&Preferences::default(), size, &[])?;
        terminal.engine.feed(
            format!(
                "\x1b[3;1H\x1b[J{}\x1b[A\x1b[2P\x1b[B\r\x1b[K",
                "a".repeat(25)
            )
            .as_bytes(),
        );
        terminal
            .engine
            .feed(b"\x1b]0;admin@gamma: ~\x07GAMMASTART\r\n");
        terminal.engine.resize(TermSize {
            columns: 15,
            rows: 10,
        })?;
        let context = terminal.engine.recent_output();
        assert!(
            !context.contains('a') && !context.contains("BETA"),
            "{context}"
        );
        Ok(())
    }

    #[test]
    fn reflow_splitting_the_cursor_line_cannot_raise_a_start_below_it() -> Result<()> {
        let narrow = TermSize {
            columns: 16,
            rows: 20,
        };
        let wide = TermSize {
            columns: 61,
            rows: 20,
        };
        let (terminal, _input) = alpha_then_beta(&Preferences::default(), narrow, &[96])?;
        terminal.engine.feed(b"\x1b[2;1H");
        terminal.engine.resize(wide)?;
        // Behind the alternate screen alacritty splits the line holding the cursor.
        terminal.engine.feed(b"\x1b[?1049h");
        terminal.engine.resize(narrow)?;
        terminal.engine.resize(wide)?;
        terminal.engine.feed(b"\x1b[?1049l");
        let context = terminal.engine.recent_output();
        assert!(!context.contains("ALPHA"), "{context}");
        Ok(())
    }

    #[test]
    fn escapes_that_end_a_control_string_still_track_the_session_start() -> Result<()> {
        for sequence in [&b"\x1b]0;x\x1b[3T\x07"[..], b"\x1bPzz\x1b[3T\x1b\\"] {
            let (terminal, _input) =
                alpha_then_beta(&Preferences::default(), TermSize::default(), &[0; 5])?;
            terminal.engine.feed(b"beta-1\r\n");
            terminal.engine.feed(sequence);
            let context = terminal.engine.recent_output();
            assert!(!context.contains("ALPHA"), "{sequence:?}: {context}");
            assert!(context.contains("BETASTART"), "{sequence:?}: {context}");
        }
        Ok(())
    }

    /// Random output, cursor jumps, clears, scroll regions, character deletion, wide text and
    /// resizes, also behind the alternate screen. The start may be lost (failing closed), but
    /// alpha's output must never reach the context.
    #[test]
    fn session_start_never_moves_above_the_switch_under_random_screen_operations() -> Result<()> {
        let mut seed: u64 = 0x1234_5678;
        let mut random = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        fn size(random: &mut impl FnMut(u64) -> u64, columns: u64, rows: u64) -> TermSize {
            TermSize {
                columns: 10 + random(columns) as u16,
                rows: 3 + random(rows) as u16,
            }
        }
        for case in 0..400 {
            let preferences = Preferences {
                scrollback: if case % 3 == 0 { 30 } else { 10_000 },
                ..Default::default()
            };
            let start = size(&mut random, 90, 30);
            let alpha: Vec<_> = (0..random(40)).map(|_| random(150) as usize).collect();
            let (terminal, _input) = alpha_then_beta(&preferences, start, &alpha)?;
            let mut operations = Vec::new();
            for _ in 0..10 {
                let operation = match random(12) {
                    0 | 1 => {
                        let size = size(&mut random, 150, 60);
                        terminal.engine.resize(size)?;
                        format!("resize {}x{}", size.columns, size.rows)
                    }
                    2 => format!("b{}\r\n", "z".repeat(random(200) as usize)),
                    3 => {
                        terminal.engine.feed(b"\x1b[?1049hfull-screen");
                        for _ in 0..=random(2) {
                            terminal.engine.resize(size(&mut random, 150, 60))?;
                        }
                        "\x1b[?1049l".into()
                    }
                    4 => format!("\x1b[{};1H", random(20) + 1),
                    5 => "\x1b[2J".into(),
                    6 => format!("\x1b[{}G\x1b[{}P", random(40) + 1, random(10) + 1),
                    7 => format!("w{}\r\n", "漢".repeat(random(80) as usize)),
                    8 => {
                        let top = random(10) + 1;
                        let bottom = top + random(15) + 1;
                        let line = top + random(4);
                        let count = random(4) + 1;
                        let kind = if random(2) == 0 { 'M' } else { 'L' };
                        format!("\x1b[{top};{bottom}r\x1b[{line};1H\x1b[{count}{kind}\x1b[r")
                    }
                    9 => format!(
                        "\x1b[{}A{}",
                        random(4) + 1,
                        "q".repeat(random(120) as usize)
                    ),
                    10 => {
                        let count = random(5) + 1;
                        let kind = if random(2) == 0 { 'S' } else { 'T' };
                        format!("\x1b[{count}{kind}")
                    }
                    _ => "\x1b[H\x1bM".into(),
                };
                if !operation.starts_with("resize") {
                    terminal.engine.feed(operation.as_bytes());
                }
                operations.push(operation);
            }
            let context = terminal.engine.recent_output();
            assert!(
                !context.contains("ALPHA"),
                "case {case}: {operations:?}\n{context}"
            );
        }
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
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
                "PTY did not exit within twenty seconds"
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
