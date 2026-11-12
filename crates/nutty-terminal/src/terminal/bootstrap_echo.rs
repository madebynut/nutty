//! Bounded, verified removal of our own SSH bootstrap echo, before normal VT parsing.

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Config, Term},
    vte::ansi::{Handler, Processor},
};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

const LIMIT: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Echo {
    wire: String,
    marker: Vec<u8>,
    bytes: Vec<u8>,
    columns: usize,
    column: usize,
    started: Instant,
}

impl Echo {
    pub fn new(source: &str, columns: usize, column: usize) -> Self {
        let token = uuid::Uuid::new_v4();
        Self {
            wire: format!(
                " printf '\\033]777;nutty-bootstrap;{token}\\007'; {}\r",
                source.trim()
            ),
            marker: format!("\x1b]777;nutty-bootstrap;{token}\x07").into_bytes(),
            bytes: Vec::new(),
            columns,
            column,
            started: Instant::now(),
        }
    }

    pub fn wire(&self) -> &str {
        &self.wire
    }

    /// An interrupted handshake reveals all buffered bytes instead of hiding diagnostics.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    pub fn filter<'a>(state: &mut Option<Self>, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(echo) = state else {
            return Cow::Borrowed(bytes);
        };
        if echo.bytes.len().saturating_add(bytes.len()) > LIMIT || echo.started.elapsed() >= TIMEOUT
        {
            let mut output = echo.flush();
            output.extend_from_slice(bytes);
            *state = None;
            return Cow::Owned(output);
        }
        echo.bytes.extend_from_slice(bytes);
        let Some(end) = memchr::memmem::find(&echo.bytes, &echo.marker) else {
            return Cow::Borrowed(&[]);
        };
        let output = echo.remove_verified_echo(end);
        *state = None;
        Cow::Owned(output)
    }

    fn remove_verified_echo(&self, end: usize) -> Vec<u8> {
        // The nonce proves which command has begun executing. Matching its entire rendered
        // input additionally prevents losing login banners, asynchronous output or typeahead.
        if self.columns > 4096 {
            return self.bytes.clone();
        }
        let before = &self.bytes[..end];
        let expected = self.wire.trim_end();
        let mut screen = scratch_screen(self.columns, self.column, before.len());
        let mut parser: Processor = Processor::new();
        let mut candidates = vec![(0, self.column)];
        for (index, byte) in before.iter().enumerate() {
            if candidates.len() < 16 && before[index..].starts_with(b" printf ") {
                candidates.push((index, screen.grid().cursor.point.column.0));
            }
            parser.advance(&mut screen, std::slice::from_ref(byte));
        }
        let mut spans = Vec::new();
        for (start, column) in candidates {
            let mut screen = scratch_screen(self.columns, column, before.len() - start);
            let mut parser: Processor = Processor::new();
            let echoed = &before[start..];
            for (offset, byte) in echoed.iter().enumerate() {
                // A shell can print the nonce before the line discipline echoes CR.
                // The command is already complete, with the cursor still on its last row.
                if *byte == b'\r' && echo_matches(&screen, column, expected) {
                    let mut stop = start + offset + 1;
                    if before.get(stop) == Some(&b'\n') {
                        stop += 1;
                    }
                    spans.push((start, stop));
                    break;
                }
                parser.advance(&mut screen, std::slice::from_ref(byte));
                if (*byte == b'\n' || offset + 1 == echoed.len())
                    && echo_matches(&screen, column, expected)
                {
                    spans.push((start, start + offset + 1));
                    break;
                }
            }
        }
        if !spans.is_empty() {
            spans.sort_unstable();
            let mut output = Vec::with_capacity(self.bytes.len());
            let mut copied = 0;
            for (start, stop) in spans {
                if start < copied {
                    continue;
                }
                output.extend_from_slice(&self.bytes[copied..start]);
                copied = stop;
            }
            output.extend_from_slice(&self.bytes[copied..end]);
            let suffix = &self.bytes[copied..end];
            let mut screen = scratch_screen(self.columns, 0, suffix.len());
            let mut parser: Processor = Processor::new();
            parser.advance(&mut screen, suffix);
            let trailing = screen.bounds_to_string(Point::default(), screen.grid().cursor.point);
            // Do not erase a real, possibly unterminated diagnostic after the echo.
            if trailing.trim().is_empty() {
                output.extend_from_slice(b"\r\x1b[2K");
            } else {
                output.extend_from_slice(b"\r\n");
            }
            output.extend_from_slice(&self.bytes[end + self.marker.len()..]);
            return output;
        }
        self.bytes.clone()
    }
}

#[derive(Clone)]
struct DiscardEvents;
impl EventListener for DiscardEvents {
    fn send_event(&self, _: Event) {}
}
struct Size {
    columns: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.columns
    }
}
type EchoScreen = Term<DiscardEvents>;

fn echo_matches(screen: &EchoScreen, column: usize, expected: &str) -> bool {
    screen
        .bounds_to_string(
            Point::new(Line(0), Column(column)),
            screen.grid().cursor.point,
        )
        .trim_end()
        == expected
}

fn scratch_screen(columns: usize, column: usize, bytes: usize) -> EchoScreen {
    let columns = columns.max(2);
    let size = Size {
        columns,
        rows: (bytes / columns + 8).min(2048),
    };
    let mut screen = Term::new(
        Config {
            scrolling_history: 0,
            ..Default::default()
        },
        &size,
        DiscardEvents,
    );
    screen.goto_col(column);
    screen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_scripts_use_one_bounded_physical_line() {
        for shell in ["bash", "sh", "dash", "ash", "zsh", "fish"] {
            let source = crate::shell_integration::remote_command(shell).expect("supported shell");
            let echo = Echo::new(&source, 100, 0);
            assert!(
                !source.contains(['\r', '\n']),
                "physical newline in {shell}"
            );
            assert!(
                echo.wire.len() < 1024,
                "{shell} exceeds canonical input limit: {} bytes",
                echo.wire.len()
            );
        }
    }

    #[test]
    fn dash_bootstrap_echo_is_removed_at_prompt_columns() {
        let source = crate::shell_integration::remote_command("dash").expect("dash");
        for width in [40, 80, 100] {
            for column in [0, 18] {
                let echo = Echo::new(&source, width, column);
                let echoed = echo.wire().replace('\r', "\r\n");
                let mut input = echoed.into_bytes();
                input.extend_from_slice(&echo.marker);
                input.extend_from_slice(b"fixture@target:~$ ");
                let mut state = Some(Echo {
                    wire: echo.wire.clone(),
                    marker: echo.marker.clone(),
                    bytes: Vec::new(),
                    columns: width,
                    column,
                    started: Instant::now(),
                });
                let output = Echo::filter(&mut state, &input);
                let text = String::from_utf8(output.to_vec()).expect("utf-8");
                assert!(
                    !text.contains("__nutty_"),
                    "width {width} column {column}: {text}"
                );
                assert!(state.is_none());

                let echo = Echo::new(&source, width, column);
                let mut early = echo.wire().trim_end().as_bytes().to_vec();
                early.extend_from_slice(&echo.marker);
                early.extend_from_slice(b"\r\nfixture@target:~$ ");
                let mut state = Some(Echo {
                    wire: echo.wire.clone(),
                    marker: echo.marker.clone(),
                    bytes: Vec::new(),
                    columns: width,
                    column,
                    started: Instant::now(),
                });
                let output = Echo::filter(&mut state, &early);
                let text = String::from_utf8(output.to_vec()).expect("utf-8");
                assert!(
                    !text.contains("__nutty_"),
                    "marker before newline, width {width} column {column}: {text}"
                );
                assert!(text.contains("fixture@target:~$"));

                let echo = Echo::new(&source, width, column);
                let mut carriage = echo.wire().trim_end().as_bytes().to_vec();
                carriage.push(b'\r');
                carriage.extend_from_slice(&echo.marker);
                carriage.extend_from_slice(b"\nfixture@target:~$ ");
                let mut state = Some(Echo {
                    wire: echo.wire.clone(),
                    marker: echo.marker.clone(),
                    bytes: Vec::new(),
                    columns: width,
                    column,
                    started: Instant::now(),
                });
                let output = Echo::filter(&mut state, &carriage);
                let text = String::from_utf8(output.to_vec()).expect("utf-8");
                assert!(
                    !text.contains("__nutty_"),
                    "carriage return before marker, width {width} column {column}: {text}"
                );
            }
        }
    }

    #[test]
    fn fragmented_echo_is_removed_but_banners_and_execution_errors_are_preserved() {
        for width in [40, 100] {
            let echo = Echo::new(r#"echo 'literal "quote" and 中文'"#, width, 0);
            let echoed = echo.wire().replace('\r', "\r\n");
            let mut input = echoed.as_bytes().to_vec();
            input.extend_from_slice(b"Login banner\r\nPassword policy warning\r\nuser@host:~$ ");
            input.extend_from_slice(echoed.as_bytes());
            input.extend_from_slice(&echo.marker);
            input.extend_from_slice(b"init error: permission denied\r\nuser@host:~$ ");
            for split in 0..=input.len() {
                let mut state = Some(Echo {
                    wire: echo.wire.clone(),
                    marker: echo.marker.clone(),
                    bytes: Vec::new(),
                    columns: width,
                    column: 0,
                    started: Instant::now(),
                });
                let mut output = Echo::filter(&mut state, &input[..split]).into_owned();
                output.extend_from_slice(&Echo::filter(&mut state, &input[split..]));
                let text = String::from_utf8(output).expect("UTF-8 fixture");
                assert!(
                    !text.contains("nutty-bootstrap"),
                    "width {width}, split {split}"
                );
                for retained in [
                    "Login banner",
                    "Password policy warning",
                    "init error: permission denied",
                ] {
                    assert!(text.contains(retained), "lost {retained}");
                }
                assert!(state.is_none());
            }
        }
    }

    #[test]
    fn interleaved_input_unknown_markers_timeouts_and_limits_fail_open() {
        let echo = Echo::new("echo init", 100, 0);
        let mut data = echo.wire.trim_end().as_bytes().to_vec();
        data.extend_from_slice(b" user typed here\r\n");
        data.extend_from_slice(&echo.marker);
        let mut state = Some(echo);
        assert_eq!(Echo::filter(&mut state, &data).as_ref(), data);
        assert!(state.is_none());

        let mut state = Some(Echo::new("echo init", 100, 0));
        let unknown = b"denied\r\n\x1b]777;nutty-bootstrap;old-token\x07";
        assert!(Echo::filter(&mut state, unknown).is_empty());
        state.as_mut().expect("pending").started = Instant::now() - TIMEOUT;
        assert_eq!(Echo::filter(&mut state, &[]).as_ref(), unknown);
        assert!(state.is_none());

        let mut state = Some(Echo::new("echo init", 100, 0));
        let large = vec![b'x'; LIMIT + 1];
        assert_eq!(Echo::filter(&mut state, &large).as_ref(), large);
        assert!(state.is_none());
        assert_eq!(
            Echo::filter(&mut state, b"normal output").as_ref(),
            b"normal output"
        );
    }

    #[test]
    fn an_unterminated_diagnostic_after_echo_remains_visible() {
        let echo = Echo::new("echo init", 100, 0);
        let mut bytes = echo.wire.replace('\r', "\r\n").into_bytes();
        bytes.extend_from_slice(b"background warning");
        bytes.extend_from_slice(&echo.marker);
        bytes.extend_from_slice(b"$ ");
        let output = Echo::filter(&mut Some(echo), &bytes).into_owned();
        let mut screen = scratch_screen(100, 0, output.len());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut screen, &output);
        let text = screen.bounds_to_string(Point::default(), screen.grid().cursor.point);
        assert!(text.contains("background warning"));
        assert!(!text.contains("nutty-bootstrap"));
    }
}
