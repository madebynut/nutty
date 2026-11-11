//! Fixed-cell GPU painting, keyboard protocols, selection, search, and native IME.

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    grid::Scroll,
    selection::SelectionType,
    term::{TermMode, cell::Flags},
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{
    App, AppContext as _, Bounds, ClipboardItem, Context, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight, IntoElement,
    KeyDownEvent, LineLayout, MouseButton, Pixels, Point, Render, ShapedLine, Subscription, Task,
    TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};

use crate::{
    config::Preferences,
    terminal::{self, Snapshot, TermSize, Terminal},
    theme::Palette,
};

#[derive(Clone, Debug)]
pub(crate) enum PaneEvent {
    Focused,
    Input(Vec<u8>),
    Paste(String),
}

struct CachedRow {
    text: String,
    runs: Vec<TextRun>,
    cells: Vec<GlyphCell>,
    line: ShapedLine,
    cell_width: Pixels,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GlyphCell {
    byte_start: usize,
    column: usize,
    columns: usize,
}

// Keep native shaping within each cell (including combining marks), then put
// its cluster at the VT grid column. Never infer columns from font advances.
fn grid_layout(
    source: &LineLayout,
    cells: &[GlyphCell],
    cell_width: Pixels,
    spacing: Pixels,
) -> LineLayout {
    let mut anchors = vec![None; cells.len()];
    for glyph in source.runs.iter().flat_map(|run| &run.glyphs) {
        let cell = cells.partition_point(|cell| cell.byte_start <= glyph.index);
        if let Some(index) = cell.checked_sub(1) {
            let anchor = &mut anchors[index];
            if anchor.is_none_or(|(byte, _)| glyph.index < byte) {
                *anchor = Some((glyph.index, glyph.position.x));
            }
        }
    }
    let mut runs = source.runs.clone();
    for glyph in runs.iter_mut().flat_map(|run| &mut run.glyphs) {
        let cell = cells.partition_point(|cell| cell.byte_start <= glyph.index);
        if let Some(index) = cell.checked_sub(1)
            && let Some((_, origin)) = anchors[index]
        {
            glyph.position.x = cell_width * cells[index].column as f32
                + spacing * cells[index].columns as f32 / 2.
                + glyph.position.x
                - origin;
        }
    }
    LineLayout {
        font_size: source.font_size,
        width: cells.last().map_or(px(0.), |cell| {
            cell_width * (cell.column + cell.columns) as f32
        }),
        ascent: source.ascent,
        descent: source.descent,
        runs,
        len: source.len,
    }
}

fn native_raster(raster: &terminal::Raster) -> Arc<gpui_kit::RenderImage> {
    let mut pixels = raster.pixels.clone();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(pixels)]))
}

struct CachedImage {
    // Keep the shared pixel budget alive for as long as the native copy is cached.
    _source: terminal::DisplayImage,
    rendered: Arc<gpui_kit::RenderImage>,
}

gpui_kit::actions!(terminal, [Complete, BackTab]);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("tab", Complete, Some("Terminal")),
        gpui_kit::KeyBinding::new("shift-tab", BackTab, Some("Terminal")),
    ]);
}

pub(crate) struct TerminalView {
    pub terminal: Terminal,
    pub snapshot: Snapshot,
    pub preferences: Preferences,
    fonts: crate::fonts::FontPair,
    focus: FocusHandle,
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    rows: Vec<CachedRow>,
    painted_generation: Option<u64>,
    rendered_images: std::collections::HashMap<uuid::Uuid, CachedImage>,
    marked: String,
    marked_selection: Range<usize>,
    search: Entity<InputState>,
    searching: bool,
    search_status: String,
    scroll_remainder: f32,
    blink_epoch: Instant,
    _cursor_refresh: Task<()>,
    _refresh: Task<()>,
    _search_subscription: Subscription,
}

impl EventEmitter<PaneEvent> for TerminalView {}

impl TerminalView {
    pub fn new(
        terminal: Terminal,
        preferences: Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let updates = terminal.updates.clone();
        let refresh = cx.spawn(async move |view, cx| {
            while updates.recv().await.is_ok() {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                while updates.try_recv().is_ok() {}
                if view
                    .update(cx, |view, cx| {
                        view.snapshot = view.terminal.engine.snapshot();
                        if view.terminal.engine.zmodem_prefix_pending() {
                            // A trailing handshake prefix must not delay regular frame updates.
                            cx.spawn(async move |view, cx| {
                                cx.background_executor()
                                    .timer(Duration::from_millis(100))
                                    .await;
                                let _ = view.update(cx, |view, _| {
                                    view.terminal.engine.flush_zmodem_prefix();
                                });
                            })
                            .detach();
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let cursor_refresh = cx.spawn_in(window, async move |view, cx| {
            loop {
                let Ok(interval) = view.update_in(cx, |view, window, cx| {
                    let blinking = view.snapshot.cursor_style.blinking
                        && !view.snapshot.closed
                        && view.focus.is_focused(window);
                    if blinking {
                        cx.notify();
                    }
                    Duration::from_millis(if blinking {
                        view.preferences.terminal.cursor_blink_ms / 2
                    } else {
                        1000
                    })
                }) else {
                    break;
                };
                cx.background_executor().timer(interval).await;
            }
        });
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::text("搜索终端 · 正则表达式"))
        });
        let search_subscription = cx.subscribe(&search, |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query = view.search.read(cx).value().to_string();
                view.search_status = match view.terminal.engine.search(&query, false) {
                    Ok(true) => String::new(),
                    Ok(false) => crate::i18n::text("没有匹配").into(),
                    Err(_) => crate::i18n::text("表达式无效").into(),
                };
                cx.notify();
            }
        });
        Self {
            snapshot: terminal.engine.snapshot(),
            terminal,
            fonts: crate::fonts::FontPair::terminal(&preferences, cx),
            preferences,
            focus: cx.focus_handle(),
            bounds: Bounds::default(),
            cell_width: px(8.),
            line_height: px(21.),
            rows: Vec::new(),
            painted_generation: None,
            rendered_images: Default::default(),
            marked: String::new(),
            marked_selection: 0..0,
            search,
            searching: false,
            search_status: String::new(),
            scroll_remainder: 0.,
            blink_epoch: Instant::now(),
            _cursor_refresh: cursor_refresh,
            _refresh: refresh,
            _search_subscription: search_subscription,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.searching = true;
        self.search.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub fn apply_preferences(
        &mut self,
        preferences: &Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preferences = preferences.clone();
        self.fonts = crate::fonts::FontPair::terminal(preferences, cx);
        self.search.update(cx, |input, cx| {
            input.set_placeholder(crate::i18n::text("搜索终端 · 正则表达式"), window, cx)
        });
        self.terminal.engine.apply_preferences(preferences);
        self.snapshot = self.terminal.engine.snapshot();
        self.blink_epoch = Instant::now();
        self.rows.clear();
        self.painted_generation = None;
        cx.notify();
    }

    fn send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.blink_epoch = Instant::now();
        if let Err(error) = self.terminal.engine.input(bytes.clone()) {
            self.terminal.engine.fail(error.to_string());
        } else {
            cx.emit(PaneEvent::Input(bytes));
        }
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.marked.is_empty() {
            return;
        }
        let key = &event.keystroke;
        let shortcut = key.modifiers.platform || (key.modifiers.control && key.modifiers.shift);
        if shortcut {
            match key.key.as_str() {
                "c" => {
                    if let Some(text) = self.terminal.engine.selection() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        match self.terminal.engine.paste(&text) {
                            Ok(()) => cx.emit(PaneEvent::Paste(text)),
                            Err(error) => self.terminal.engine.fail(error.to_string()),
                        }
                    }
                }
                "f" => {
                    self.searching = true;
                    self.search.update(cx, |input, cx| input.focus(window, cx));
                    cx.notify();
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        // Let the native input handler compose Option characters when requested.
        if cfg!(target_os = "macos")
            && key.modifiers.alt
            && !self.preferences.terminal.option_as_alt
        {
            return;
        }
        if let Some(bytes) = key_bytes(key, self.snapshot.mode) {
            self.send(bytes, cx);
            cx.stop_propagation();
        }
    }

    fn coordinates(&self, position: Point<Pixels>) -> (usize, usize) {
        let x = f32::from(position.x - self.bounds.left()).max(0.);
        let y = f32::from(position.y - self.bounds.top()).max(0.);
        (
            ((x / f32::from(self.cell_width)) as usize)
                .min(self.snapshot.columns.saturating_sub(1)),
            ((y / f32::from(self.line_height)) as usize).min(self.snapshot.rows.saturating_sub(1)),
        )
    }

    fn mouse_report(
        &mut self,
        position: Point<Pixels>,
        button: u8,
        released: bool,
        cx: &mut Context<Self>,
    ) {
        let (column, row) = self.coordinates(position);
        let bytes = if self.snapshot.mode.contains(TermMode::SGR_MOUSE) {
            format!(
                "\x1b[<{button};{};{}{}",
                column + 1,
                row + 1,
                if released { 'm' } else { 'M' }
            )
            .into_bytes()
        } else {
            let code = if released { 3 } else { button };
            vec![
                27,
                b'[',
                b'M',
                code + 32,
                (column + 33).min(255) as u8,
                (row + 33).min(255) as u8,
            ]
        };
        self.send(bytes, cx);
    }

    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.bounds = bounds;
        let font = &self.fonts.latin;
        let text_system = window.text_system();
        let font_size = px(self.preferences.font_size);
        let latin = text_system.resolve_font(font);
        let chinese = text_system.resolve_font(&self.fonts.chinese);
        self.cell_width = ['M', 'W', '@', '0']
            .into_iter()
            .filter_map(|ch| {
                text_system
                    .advance(latin, font_size, ch)
                    .ok()
                    .map(|advance| advance.width)
            })
            .chain(['中', '国', 'Ｍ'].into_iter().filter_map(|ch| {
                text_system
                    .advance(chinese, font_size, ch)
                    .ok()
                    .map(|advance| advance.width / 2.)
            }))
            .fold(px(self.preferences.font_size * 0.6), |width, advance| {
                width.max(advance)
            });
        self.cell_width += px(self.preferences.terminal.letter_spacing);
        let natural_height = [latin, chinese]
            .into_iter()
            .map(|id| text_system.ascent(id, font_size) + text_system.descent(id, font_size))
            .fold(px(0.), Pixels::max);
        self.line_height =
            px((self.preferences.font_size * self.preferences.terminal.line_height).ceil())
                .max(natural_height);
        self.terminal
            .engine
            .set_cell_size(f32::from(self.cell_width), f32::from(self.line_height));
        let size = TermSize {
            columns: (f32::from(bounds.size.width) / f32::from(self.cell_width)).clamp(2., 1000.)
                as u16,
            rows: (f32::from(bounds.size.height) / f32::from(self.line_height)).clamp(1., 500.)
                as u16,
        };
        if usize::from(size.columns) != self.snapshot.columns
            || usize::from(size.rows) != self.snapshot.rows
        {
            if let Err(error) = self.terminal.engine.resize(size) {
                self.terminal.engine.fail(error.to_string());
            }
            self.snapshot = self.terminal.engine.snapshot();
        }
        if self.painted_generation == Some(self.snapshot.generation) {
            return;
        }
        let palette = Palette::for_appearance(self.preferences.appearance);
        let mut text = vec![String::new(); self.snapshot.rows];
        let mut runs: Vec<Vec<TextRun>> = vec![Vec::new(); self.snapshot.rows];
        let mut cells: Vec<Vec<GlyphCell>> = vec![Vec::new(); self.snapshot.rows];
        for indexed in &self.snapshot.cells {
            let cell = &indexed.cell;
            let mut content = terminal::cell_text(cell);
            if content.is_empty() {
                continue;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                content = " ".repeat(if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                });
            }
            let mut fg = terminal::resolve_color(cell.fg, palette, true);
            let bg = terminal::resolve_color(cell.bg, palette, false);
            if cell.flags.contains(Flags::INVERSE) {
                fg = bg;
            }
            let mut font = self.fonts.for_text(&content).clone();
            if cell.flags.contains(Flags::BOLD) {
                font.weight = FontWeight::BOLD;
            }
            if cell.flags.contains(Flags::ITALIC) {
                font.style = FontStyle::Italic;
            }
            let underline = cell
                .flags
                .intersects(Flags::UNDERLINE | Flags::DOUBLE_UNDERLINE | Flags::UNDERCURL)
                .then_some(UnderlineStyle {
                    color: Some(rgb(fg).into()),
                    thickness: px(1.),
                    wavy: cell.flags.contains(Flags::UNDERCURL),
                });
            let run = TextRun {
                len: content.len(),
                font,
                color: rgb(fg).into(),
                background_color: None,
                underline,
                strikethrough: None,
            };
            cells[indexed.row].push(GlyphCell {
                byte_start: text[indexed.row].len(),
                column: indexed.column,
                columns: if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                },
            });
            if let Some(last) = runs[indexed.row].last_mut()
                && last.font == run.font
                && last.color == run.color
                && last.underline == run.underline
            {
                last.len += run.len;
            } else {
                runs[indexed.row].push(run);
            }
            text[indexed.row].push_str(&content);
        }
        let mut previous = std::mem::take(&mut self.rows).into_iter();
        self.rows = text
            .into_iter()
            .zip(runs)
            .zip(cells)
            .map(|((text, runs), cells)| {
                if let Some(row) = previous.next()
                    && row.text == text
                    && row.runs == runs
                    && row.cells == cells
                    && row.cell_width == self.cell_width
                {
                    return row;
                }
                let mut line = text_system.shape_line(
                    text.clone().into(),
                    px(self.preferences.font_size),
                    &runs,
                    None,
                );
                *line = Arc::new(grid_layout(
                    &line,
                    &cells,
                    self.cell_width,
                    px(self.preferences.terminal.letter_spacing),
                ));
                CachedRow {
                    text,
                    runs,
                    cells,
                    line,
                    cell_width: self.cell_width,
                }
            })
            .collect();
        self.painted_generation = Some(self.snapshot.generation);
        cx.notify();
    }

    fn paint_images(&self, front: bool, window: &mut Window) {
        for image in self
            .snapshot
            .images
            .iter()
            .filter(|image| (image.z >= 0) == front)
        {
            let Some(rendered) = self.rendered_images.get(&image.image.id) else {
                continue;
            };
            let iw = image.image.pixels.width() as f32;
            let ih = image.image.pixels.height() as f32;
            let [x, y, width, height] = image.crop;
            if width <= 0. || height <= 0. {
                continue;
            }
            let bounds = Bounds::new(
                point(
                    self.bounds.left() + self.cell_width * image.column,
                    self.bounds.top() + self.line_height * image.row,
                ),
                size(
                    self.cell_width * image.columns,
                    self.line_height * image.rows,
                ),
            );
            let scale_x = f32::from(bounds.size.width) / width;
            let scale_y = f32::from(bounds.size.height) / height;
            let image_bounds = Bounds::new(
                point(
                    bounds.left() - px(x * scale_x),
                    bounds.top() - px(y * scale_y),
                ),
                size(px(iw * scale_x), px(ih * scale_y)),
            );
            if let Err(error) = window.paint_image(
                bounds.intersect(&self.bounds),
                image_bounds,
                gpui_kit::Corners::default(),
                rendered.rendered.clone(),
                0,
                false,
            ) {
                tracing::warn!(%error,"terminal image painting failed");
            }
        }
    }
    fn paint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rendered_images.retain(|id, _| {
            self.snapshot
                .images
                .iter()
                .any(|image| image.image.id == *id)
        });
        for image in &self.snapshot.images {
            self.rendered_images
                .entry(image.image.id)
                .or_insert_with(|| CachedImage {
                    _source: image.clone(),
                    rendered: native_raster(&image.image),
                });
        }
        let palette = Palette::for_appearance(self.preferences.appearance);
        for indexed in &self.snapshot.cells {
            let mut bg = terminal::resolve_color(indexed.cell.bg, palette, false);
            if indexed.cell.flags.contains(Flags::INVERSE) {
                bg = terminal::resolve_color(indexed.cell.fg, palette, true);
            }
            if indexed.selected {
                bg = palette.hover;
            }
            if bg != palette.terminal {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            self.bounds.left() + self.cell_width * indexed.column as f32,
                            self.bounds.top() + self.line_height * indexed.row as f32,
                        ),
                        size(self.cell_width, self.line_height),
                    ),
                    rgb(bg),
                ));
            }
        }
        self.paint_images(false, window);
        for (row, cached) in self.rows.iter().enumerate() {
            if let Err(error) = cached.line.paint(
                point(
                    self.bounds.left(),
                    self.bounds.top() + self.line_height * row as f32,
                ),
                self.line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            ) {
                tracing::error!(%error,"terminal row painting failed");
            }
        }
        self.paint_images(true, window);
        if let Some((column, row)) = self.snapshot.cursor {
            let origin = point(
                self.bounds.left() + self.cell_width * column as f32,
                self.bounds.top() + self.line_height * row as f32,
            );
            let wide = self.snapshot.cells.iter().any(|cell| {
                cell.column == column
                    && cell.row == row
                    && cell.cell.flags.contains(Flags::WIDE_CHAR)
            });
            let width = self.cell_width * if wide { 2. } else { 1. };
            let cursor = Bounds::new(origin, size(width, self.line_height));
            if !self.snapshot.closed {
                if self.focus.is_focused(window) {
                    let visible = !self.snapshot.cursor_style.blinking
                        || !self.marked.is_empty()
                        || (self.blink_epoch.elapsed().as_millis()
                            / u128::from(self.preferences.terminal.cursor_blink_ms))
                        .is_multiple_of(2);
                    if visible {
                        use alacritty_terminal::vte::ansi::CursorShape;
                        let thickness = px(self.preferences.terminal.cursor_width);
                        match self.snapshot.cursor_style.shape {
                            CursorShape::Block => {
                                window.paint_quad(fill(cursor, rgb(palette.accent).opacity(0.4)))
                            }
                            CursorShape::Beam => window.paint_quad(fill(
                                Bounds::new(origin, size(thickness, self.line_height)),
                                rgb(palette.accent),
                            )),
                            CursorShape::Underline => window.paint_quad(fill(
                                Bounds::new(
                                    point(origin.x, cursor.bottom() - thickness),
                                    size(width, thickness),
                                ),
                                rgb(palette.accent),
                            )),
                            CursorShape::HollowBlock => window.paint_quad(gpui_kit::outline(
                                cursor,
                                rgb(palette.accent),
                                gpui_kit::BorderStyle::Solid,
                            )),
                            CursorShape::Hidden => {}
                        }
                    }
                } else if self.preferences.terminal.unfocused_cursor {
                    window.paint_quad(gpui_kit::outline(
                        cursor,
                        rgb(palette.muted),
                        gpui_kit::BorderStyle::Solid,
                    ));
                }
            }
            if !self.marked.is_empty() {
                let run = TextRun {
                    len: self.marked.len(),
                    font: self.fonts.for_text(&self.marked).clone(),
                    color: rgb(palette.text).into(),
                    background_color: Some(rgb(palette.surface).into()),
                    underline: Some(UnderlineStyle {
                        color: Some(rgb(palette.accent).into()),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                let line = window.text_system().shape_line(
                    self.marked.clone().into(),
                    px(self.preferences.font_size),
                    &[run],
                    None,
                );
                let _ = line.paint(origin, self.line_height, TextAlign::Left, None, window, cx);
            }
        }
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(self.bounds, cx.entity()),
            cx,
        );
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let palette = Palette::for_appearance(self.preferences.appearance);
        let search =
            self.searching.then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .bg(rgb(palette.surface))
                    .child(crate::controls::input(&self.search, cx).w_full())
                    .child(self.search_status.clone())
                    .child(div().id("next-match").cursor_pointer().child("↓").on_click(
                        cx.listener(|view, _, _, cx| {
                            let query = view.search.read(cx).value().to_string();
                            let _ = view.terminal.engine.search(&query, true);
                        }),
                    ))
                    .child(
                        div()
                            .id("close-search")
                            .cursor_pointer()
                            .child("×")
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.searching = false;
                                view.focus(window, cx);
                                cx.notify();
                            })),
                    )
            });
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(rgb(palette.terminal))
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_action(
                cx.listener(|view, _: &gpui_kit::component::input::Copy, _, cx| {
                    if let Some(text) = view.terminal.engine.selection() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }),
            )
            .on_action(
                cx.listener(|view, _: &gpui_kit::component::input::Paste, _, cx| {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        match view.terminal.engine.paste(&text) {
                            Ok(()) => cx.emit(PaneEvent::Paste(text)),
                            Err(error) => view.terminal.engine.fail(error.to_string()),
                        }
                    }
                }),
            )
            .on_action(
                cx.listener(|view, _: &gpui_kit::component::input::SelectAll, _, _| {
                    view.terminal.engine.select_all()
                }),
            )
            .on_key_down(cx.listener(Self::key))
            .on_action(cx.listener(|view, _: &Complete, _, cx| {
                view.send(b"\t".to_vec(), cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|view, _: &BackTab, _, cx| {
                view.send(b"\x1b[Z".to_vec(), cx);
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &gpui_kit::MouseDownEvent, window, cx| {
                    view.focus(window, cx);
                    cx.emit(PaneEvent::Focused);
                    if view.snapshot.mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift
                    {
                        view.mouse_report(event.position, 0, false, cx);
                    } else {
                        let (column, row) = view.coordinates(event.position);
                        view.terminal.engine.select(
                            column,
                            row,
                            event.modifiers.shift,
                            match event.click_count {
                                2 => SelectionType::Semantic,
                                3 => SelectionType::Lines,
                                _ => SelectionType::Simple,
                            },
                        );
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event: &gpui_kit::MouseUpEvent, _, cx| {
                    if view.snapshot.mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift
                    {
                        view.mouse_report(event.position, 0, true, cx);
                    } else if view.preferences.terminal.copy_on_select
                        && let Some(text) = view.terminal.engine.selection()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }),
            )
            .on_mouse_move(
                cx.listener(|view, event: &gpui_kit::MouseMoveEvent, _, cx| {
                    if view
                        .snapshot
                        .mode
                        .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
                        && !event.modifiers.shift
                    {
                        if event.dragging() {
                            view.mouse_report(event.position, 32, false, cx);
                        } else if view.snapshot.mode.contains(TermMode::MOUSE_MOTION) {
                            view.mouse_report(event.position, 35, false, cx);
                        }
                    } else if event.dragging() {
                        let (column, row) = view.coordinates(event.position);
                        view.terminal
                            .engine
                            .select(column, row, true, SelectionType::Simple);
                    }
                }),
            )
            .on_scroll(cx.listener(|view, event: &gpui_kit::ScrollEvent, _, cx| {
                view.scroll_remainder += f32::from(event.delta.pixel_delta(view.line_height).y)
                    * view.preferences.terminal.scroll_speed
                    / f32::from(view.line_height);
                let lines = view.scroll_remainder.trunc() as i32;
                view.scroll_remainder -= lines as f32;
                if view.snapshot.mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
                    for _ in 0..lines.unsigned_abs().min(100) {
                        view.mouse_report(
                            event.position,
                            if lines > 0 { 64 } else { 65 },
                            false,
                            cx,
                        );
                    }
                } else if view.snapshot.mode.contains(TermMode::ALT_SCREEN) {
                    for _ in 0..lines.unsigned_abs().min(100) {
                        view.send(
                            if lines > 0 {
                                b"\x1b[A".to_vec()
                            } else {
                                b"\x1b[B".to_vec()
                            },
                            cx,
                        );
                    }
                } else {
                    view.terminal.engine.scroll(Scroll::Delta(lines));
                }
                cx.stop_propagation();
            }))
            .children(search)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .px(px(self.preferences.terminal.padding_horizontal))
                    .py(px(self.preferences.terminal.padding_vertical))
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                entity.update(cx, |view, cx| view.layout(bounds, window, cx));
                            },
                            move |_, (), window, cx| {
                                paint_entity.update(cx, |view, cx| view.paint(window, cx));
                            },
                        )
                        .size_full(),
                    ),
            )
    }
}

pub(crate) fn key_bytes(key: &gpui_kit::Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let code = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control);
    let cursor = |suffix: char| {
        if code > 1 {
            format!("\x1b[1;{code}{suffix}")
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{suffix}")
        } else {
            format!("\x1b[{suffix}")
        }
    };
    let tilde = |number: u8| {
        if code > 1 {
            format!("\x1b[{number};{code}~")
        } else {
            format!("\x1b[{number}~")
        }
    };
    let text = match key.key.as_str() {
        "enter" => "\r".into(),
        "backspace" => if modifiers.alt { "\x1b\x7f" } else { "\x7f" }.into(),
        "escape" => "\x1b".into(),
        "tab" => if modifiers.shift { "\x1b[Z" } else { "\t" }.into(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => "\x1bOP".into(),
        "f2" => "\x1bOQ".into(),
        "f3" => "\x1bOR".into(),
        "f4" => "\x1bOS".into(),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        _ if modifiers.control => {
            let character = key.key.chars().next()?;
            let value = match character {
                'a'..='z' => character as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 27,
                '\\' | '4' => 28,
                ']' | '5' => 29,
                '^' | '6' => 30,
                '_' | '7' => 31,
                '?' | '8' => 127,
                _ => return None,
            };
            return Some(if modifiers.alt {
                vec![27, value]
            } else {
                vec![value]
            });
        }
        _ if modifiers.alt => format!("\x1b{}", key.key_char.as_deref().unwrap_or(&key.key)),
        _ => return None,
    };
    Some(text.into_bytes())
}

fn utf16_slice(text: &str, range: Range<usize>) -> Option<String> {
    let units: Vec<_> = text.encode_utf16().collect();
    String::from_utf16(units.get(range)?).ok()
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        *actual = Some(range.clone());
        utf16_slice(&self.marked, range)
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.marked.is_empty()).then_some(0..self.marked.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        self.marked_selection = 0..0;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.marked_selection = 0..0;
        self.send(text.as_bytes().to_vec(), cx);
        window.notify_text_input_changed(&self.focus);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = text.into();
        let length = text.encode_utf16().count();
        self.marked_selection = selected.unwrap_or(length..length);
        window.notify_text_input_changed(&self.focus);
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (column, row) = self.snapshot.cursor?;
        Some(Bounds::new(
            point(
                self.bounds.left() + self.cell_width * column as f32,
                self.bounds.top() + self.line_height * row as f32,
            ),
            size(self.cell_width, self.line_height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.marked.encode_utf16().count())
    }
    fn set_selected_text_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_selection = range;
        window.notify_text_selection_changed(&self.focus);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_encodes_shell_controls_and_application_cursor_modes() -> anyhow::Result<()> {
        for (binding, mode, expected) in [
            ("tab", TermMode::empty(), b"\t".as_slice()),
            ("ctrl-c", TermMode::empty(), b"\x03".as_slice()),
            ("up", TermMode::APP_CURSOR, b"\x1bOA"),
            ("ctrl-left", TermMode::empty(), b"\x1b[1;5D"),
            ("shift-tab", TermMode::empty(), b"\x1b[Z"),
        ] {
            let key = gpui_kit::Keystroke::parse(binding)?;
            assert_eq!(
                key_bytes(&key, mode).as_deref(),
                Some(expected),
                "binding {binding}"
            );
        }
        assert_eq!(utf16_slice("a😀中", 1..3), Some("😀".into()));
        assert_eq!(utf16_slice("a😀中", 1..2), None);
        Ok(())
    }

    #[test]
    fn grid_positions_keep_wide_characters_gaps_and_combining_marks() {
        let source = LineLayout {
            width: px(32.),
            len: "A中e\u{301}B".len(),
            runs: vec![gpui_kit::ShapedRun {
                font_id: gpui_kit::FontId(0),
                glyphs: [(0, 0.), (1, 8.), (4, 16.), (5, 18.), (7, 24.)]
                    .into_iter()
                    .map(|(index, x)| gpui_kit::ShapedGlyph {
                        id: gpui_kit::GlyphId(0),
                        index,
                        position: point(px(x), px(0.)),
                        is_emoji: false,
                    })
                    .collect(),
            }],
            ..Default::default()
        };
        let cells =
            [(0, 0, 1), (1, 1, 2), (4, 3, 1), (7, 8, 1)].map(|(byte_start, column, columns)| {
                GlyphCell {
                    byte_start,
                    column,
                    columns,
                }
            });
        let positioned = grid_layout(&source, &cells, px(8.), px(0.));
        let positions: Vec<_> = positioned.runs[0]
            .glyphs
            .iter()
            .map(|glyph| f32::from(glyph.position.x))
            .collect();
        assert_eq!(
            positions,
            [0., 8., 24., 26., 64.],
            "Chinese occupies two columns and a combining mark remains attached to its base"
        );
        assert_eq!(
            positioned.width,
            px(72.),
            "cursor-addressed empty columns must survive shaping"
        );
        let spaced = grid_layout(&source, &cells, px(10.), px(2.));
        let positions: Vec<_> = spaced.runs[0]
            .glyphs
            .iter()
            .map(|glyph| f32::from(glyph.position.x))
            .collect();
        assert_eq!(
            positions,
            [1., 12., 31., 33., 81.],
            "letter spacing pads each cell without detaching combining marks or compressing empty columns"
        );
        assert_eq!(
            source.runs[0].glyphs[4].position.x,
            px(24.),
            "the shared shaping cache must remain unchanged"
        );
    }
}

#[cfg(all(test, unix))]
mod completion_tests {
    use super::*;
    use anyhow::{Context as _, Result, ensure};
    use nutty_core::config::Preferences;
    use nutty_terminal::terminal::Terminal;
    use std::thread;

    #[test]
    fn tab_completes_a_filename_in_a_real_shell() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        let launcher = directory.path().join("completion-shell");
        std::fs::write(
            &launcher,
            "#!/bin/sh\nexec /bin/bash --noprofile --norc -i\n",
        )?;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
        std::fs::write(
            directory.path().join("nutty-completion-target.txt"),
            "NUTTY_TAB_COMPLETION_OK\n",
        )?;
        let terminal = Terminal::local(&Preferences {
            shell: launcher,
            directory: directory.path().to_owned(),
            track_directory: false,
            ..Default::default()
        })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal.engine.recent_output().contains("bash-") {
            ensure!(
                std::time::Instant::now() < deadline,
                "isolated bash did not show its prompt"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        terminal.engine.input(b"cat nutty-completion-".to_vec())?;
        let tab = super::key_bytes(&gpui_kit::Keystroke::parse("tab")?, TermMode::empty())
            .context("Tab must encode a terminal input byte")?;
        terminal.engine.input(tab)?;
        terminal.engine.input(b"\r".to_vec())?;
        loop {
            if terminal
                .engine
                .recent_output()
                .contains("NUTTY_TAB_COMPLETION_OK")
            {
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "Tab did not complete the filename in bash"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        terminal.engine.close_input();
        Ok(())
    }
}

#[cfg(test)]
mod raster_tests {
    use super::*;
    #[test]
    fn rgba_rasters_are_adapted_to_the_native_bgra_atlas_without_changing_alpha() {
        let raster = terminal::Raster {
            id: uuid::Uuid::new_v4(),
            pixels: image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 123, 0, 0, 255, 234])
                .expect("two pixels"),
        };
        let native = native_raster(&raster);
        assert_eq!(
            native.as_bytes(0),
            Some([0, 0, 255, 123, 255, 0, 0, 234].as_slice())
        );
        assert_eq!(
            raster.pixels.as_raw(),
            &[255, 0, 0, 123, 0, 0, 255, 234],
            "native conversion preserves shared source pixels"
        );
    }
}
