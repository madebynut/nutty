//! Where the current target session's output begins in the primary grid. The row follows
//! scrolling, clearing and reflow, so model context never reaches back into the output of
//! another host shown earlier in the same pane.
use alacritty_terminal::{
    grid::{Dimensions as _, GridCell as _},
    index::{Column, Line},
    term::{Term, TermMode, cell::Flags},
};

/// Rows of recent output that model context reads back from the bottom of the screen.
pub(super) const CONTEXT_ROWS: i32 = 120;

#[derive(Debug, Default)]
pub(super) struct SessionStart {
    /// Primary-screen row of the session's first line; negative rows are scrollback. The row
    /// just below the screen means the session starts after every current row.
    row: Option<i32>,
    /// A switch seen while a full-screen program owns the display; marked on return.
    pending: bool,
    /// Where the start was when the alternate screen opened. alacritty reflows the hidden
    /// primary grid with its cursor on resize.
    hidden: Option<Anchor>,
    /// Whether the hidden primary grid was resized, and whether any resize changed its width.
    hidden_resize: Option<bool>,
}

/// Position of the start relative to the cursor, in logical (unwrapped) lines. On a width
/// change alacritty's reflow keeps logical lines, except that it can split the cursor's line
/// and join a line ending in a stale wrap flag with the next one.
#[derive(Debug)]
pub(super) enum Anchor {
    /// The start begins the `lines`-th line above the cursor's. `joins` of the lines ending
    /// between the start's predecessor and the cursor's line could be joined with the next.
    Above {
        lines: usize,
        joins: usize,
    },
    Below(usize),
    /// The session starts after every current row.
    After,
}

impl SessionStart {
    /// Begins a new session at the start of the logical line holding the cursor.
    pub fn mark<T>(&mut self, term: &Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.pending = true;
            return;
        }
        self.pending = false;
        let top = term.topmost_line().0;
        let mut line = term.grid().cursor.point.line.0;
        while line > top && wrapped(term, line - 1) {
            line -= 1;
        }
        self.row = Some(line);
    }

    /// Starts a new session at the cursor after the output currently being written.
    pub fn defer(&mut self) {
        self.pending = true;
    }

    /// Applies a switch deferred by the alternate screen once the primary screen is back.
    pub fn settle<T>(&mut self, term: &Term<T>) {
        if self.pending && !term.mode().contains(TermMode::ALT_SCREEN) {
            self.mark(term);
        }
    }

    /// First primary row of the current session while older rows are still retained.
    /// Callers must only use it while the primary screen is active.
    pub fn row<T>(&self, term: &Term<T>) -> Option<i32> {
        self.row.filter(|row| *row > term.topmost_line().0)
    }

    pub fn active(&self) -> bool {
        self.row.is_some()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Mirrors a primary-screen scroll of `amount` rows (positive moves text up).
    pub fn scroll<T>(&mut self, term: &Term<T>, top: i32, bottom: i32, amount: i32) {
        let Some(row) = &mut self.row else {
            return;
        };
        if term.mode().contains(TermMode::ALT_SCREEN) || amount == 0 || top > bottom {
            return;
        }
        let lines = term.screen_lines() as i32;
        if top == 0 && amount > 0 {
            // A region anchored at the top rotates scrollback with it. Past the end of
            // scrollback the start is gone, and `row` reports every retained row as newer.
            // A start after every row begins with the rows scrolled in at the screen bottom.
            if *row <= bottom || (*row == lines && bottom == lines - 1) {
                *row = row.saturating_sub(amount);
            }
        } else if (top..=bottom).contains(row) {
            // Pushed out at the bottom, the start leaves only older rows in the region.
            *row = (*row - amount).clamp(top, bottom + 1);
        }
    }

    /// `ESC [2J` on the primary screen pushes the occupied rows into scrollback first.
    pub fn clear_viewport<T>(&mut self, term: &Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let lines = term.screen_lines();
        if self.row == Some(lines as i32) {
            // Every screen row is blank afterwards, so anything written there is new output.
            self.row = Some(0);
            return;
        }
        let occupied = (0..lines as i32)
            .rev()
            .find(|line| {
                (0..term.columns())
                    .any(|column| !term.grid()[Line(*line)][Column(column)].is_empty())
            })
            .map_or(0, |line| line + 1);
        self.scroll(term, 0, lines as i32 - 1, occupied);
    }

    /// `ESC [3J` drops the active grid's scrollback; a session that started there now
    /// covers everything. In the alternate screen the primary scrollback is untouched.
    pub fn clear_history<T>(&mut self, term: &Term<T>) {
        if !term.mode().contains(TermMode::ALT_SCREEN) && self.row.is_some_and(|row| row < 0) {
            self.row = None;
        }
    }

    /// Anchors the start to the cursor, as reflow keeps logical lines around it. Called just
    /// before the alternate screen opens, while the primary grid is visible.
    pub fn hide<T>(&mut self, term: &Term<T>) {
        self.hidden = self.before_resize(term);
        self.hidden_resize = None;
    }

    /// Called right after the primary screen returns.
    pub fn reveal<T>(&mut self, term: &Term<T>) {
        let anchor = self.hidden.take();
        // Without a resize the hidden primary grid is exactly as it was.
        if let Some(reflowed) = self.hidden_resize.take() {
            self.after_resize(term, anchor, reflowed);
        }
        self.settle(term);
    }

    /// Anchors never point above the start: a start inside a wrapped line moves to the next
    /// line, and one farther back than [`CONTEXT_ROWS`] lines moves to that line, which still
    /// keeps every context row in the session because a line spans at least one row.
    pub fn before_resize<T>(&self, term: &Term<T>) -> Option<Anchor> {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let start = self.row(term)?;
        if start >= term.screen_lines() as i32 {
            return Some(Anchor::After);
        }
        let cursor = term.grid().cursor.point.line.0;
        let starts_line = |line: i32| !wrapped(term, line - 1);
        if start <= cursor {
            let (mut starts, mut joins) = (0, 0);
            for line in (start..=cursor).rev() {
                if starts_line(line) {
                    starts += 1;
                    joins += usize::from(may_join(term, line - 1));
                    if starts > CONTEXT_ROWS as usize {
                        break;
                    }
                }
            }
            Some(match starts.checked_sub(1) {
                Some(lines) => Anchor::Above { lines, joins },
                None => Anchor::After,
            })
        } else {
            let starts = ((cursor + 1)..=start)
                .filter(|line| starts_line(*line))
                .count();
            Some(Anchor::Below(starts + usize::from(!starts_line(start))))
        }
    }

    /// `reflowed` is whether the width changed since the anchor was taken.
    pub fn after_resize<T>(&mut self, term: &Term<T>, anchor: Option<Anchor>, reflowed: bool) {
        // The alternate grid says nothing about the primary rows; keep the start as is.
        if term.mode().contains(TermMode::ALT_SCREEN) {
            self.hidden_resize = Some(self.hidden_resize.unwrap_or(false) || reflowed);
            return;
        }
        let top = term.topmost_line().0;
        let bottom = term.screen_lines() as i32 - 1;
        let cursor = term.grid().cursor.point.line.0;
        let starts_line = |line: i32| line == top || !wrapped(term, line - 1);
        match anchor {
            None => {
                if self.row.is_some() && self.row(term).is_none() {
                    self.row = None;
                }
            }
            Some(Anchor::After) => self.row = Some(bottom + 1),
            Some(Anchor::Above { lines, joins }) => {
                // Each join removes one line between the start and the cursor; assuming every
                // possible join happened can only place the start lower.
                let Some(logical) = (if reflowed {
                    lines.checked_sub(joins)
                } else {
                    Some(lines)
                }) else {
                    self.row = Some(bottom + 1);
                    return;
                };
                let mut seen = 0;
                for line in (top..=cursor).rev() {
                    if starts_line(line) {
                        if seen == logical {
                            self.row = Some(line);
                            return;
                        }
                        seen += 1;
                    }
                }
                // The start reflowed past the end of scrollback.
                self.row = None;
            }
            // Splitting the cursor's line adds line starts below the cursor, which would move
            // the start up.
            Some(Anchor::Below(_)) if reflowed => {
                self.row = Some(bottom + 1);
            }
            Some(Anchor::Below(logical)) => {
                let mut seen = 0;
                for line in (cursor + 1)..=bottom {
                    if starts_line(line) {
                        seen += 1;
                        if seen == logical {
                            self.row = Some(line);
                            return;
                        }
                    }
                }
                // Rows below the cursor were dropped; failing closed, no current row belongs
                // to the session.
                self.row = Some(bottom + 1);
            }
        }
    }
}

/// Whether reflow may continue the logical line ending at `line` into the next one, as
/// alacritty does when its last occupied cell carries a wrap or leading wide-char spacer flag.
/// Character deletion and earlier reflows leave such flags away from the last column.
fn may_join<T>(term: &Term<T>, line: i32) -> bool {
    let row = &term.grid()[Line(line)];
    (0..term.columns())
        .rev()
        .map(|column| &row[Column(column)])
        .find(|cell| !cell.is_empty())
        .is_some_and(|cell| {
            cell.flags
                .intersects(Flags::WRAPLINE | Flags::LEADING_WIDE_CHAR_SPACER)
        })
}

fn wrapped<T>(term: &Term<T>, line: i32) -> bool {
    term.grid()[Line(line)][Column(term.columns() - 1)]
        .flags
        .contains(Flags::WRAPLINE)
}
