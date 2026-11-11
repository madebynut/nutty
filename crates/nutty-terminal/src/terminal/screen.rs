//! VT forwarding with image anchors updated at the exact scrolling operation.

use cursor_icon::CursorIcon;

use super::{Listener, graphics::Graphics};
use alacritty_terminal::{
    grid::Dimensions as _,
    term::{Term, TermMode},
    vte::ansi::{
        Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
        KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
        ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
    },
};

pub(super) struct Screen<'a> {
    pub term: &'a mut Term<Listener>,
    pub graphics: &'a mut Graphics,
}
impl Screen<'_> {
    fn region(&self) -> (i32, i32) {
        self.graphics
            .region
            .unwrap_or((0, self.term.screen_lines() as i32 - 1))
    }
    fn scroll(&mut self, top: i32, bottom: i32, count: i32) {
        self.graphics.scroll(
            top,
            bottom,
            count,
            self.term.mode().contains(TermMode::ALT_SCREEN),
            self.term.history_size(),
            self.term.screen_lines(),
        );
    }
    fn line_scroll(&mut self) {
        if !self.graphics.has_placements() {
            return;
        }
        let (top, bottom) = self.region();
        if self.term.grid().cursor.point.line.0 == bottom {
            self.scroll(top, bottom, 1);
        }
    }
}
impl Handler for Screen<'_> {
    fn set_title(&mut self, v0: Option<String>) {
        self.term.set_title(v0);
    }
    fn set_cursor_style(&mut self, v0: Option<CursorStyle>) {
        self.term.set_cursor_style(v0);
    }
    fn set_cursor_shape(&mut self, v0: CursorShape) {
        self.term.set_cursor_shape(v0);
    }
    fn input(&mut self, v0: char) {
        if self.term.grid().cursor.input_needs_wrap
            && self.term.mode().contains(TermMode::LINE_WRAP)
        {
            self.line_scroll();
        }
        self.term.input(v0);
    }
    fn goto(&mut self, v0: i32, v1: usize) {
        self.term.goto(v0, v1);
    }
    fn goto_line(&mut self, v0: i32) {
        self.term.goto_line(v0);
    }
    fn goto_col(&mut self, v0: usize) {
        self.term.goto_col(v0);
    }
    fn insert_blank(&mut self, v0: usize) {
        self.term.insert_blank(v0);
    }
    fn move_up(&mut self, v0: usize) {
        self.term.move_up(v0);
    }
    fn move_down(&mut self, v0: usize) {
        self.term.move_down(v0);
    }
    fn identify_terminal(&mut self, v0: Option<char>) {
        self.term.identify_terminal(v0);
    }
    fn device_status(&mut self, v0: usize) {
        self.term.device_status(v0);
    }
    fn move_forward(&mut self, v0: usize) {
        self.term.move_forward(v0);
    }
    fn move_backward(&mut self, v0: usize) {
        self.term.move_backward(v0);
    }
    fn move_down_and_cr(&mut self, v0: usize) {
        self.term.move_down_and_cr(v0);
    }
    fn move_up_and_cr(&mut self, v0: usize) {
        self.term.move_up_and_cr(v0);
    }
    fn put_tab(&mut self, v0: u16) {
        self.term.put_tab(v0);
    }
    fn backspace(&mut self) {
        self.term.backspace();
    }
    fn carriage_return(&mut self) {
        self.term.carriage_return();
    }
    fn linefeed(&mut self) {
        self.line_scroll();
        self.term.linefeed();
    }
    fn bell(&mut self) {
        self.term.bell();
    }
    fn substitute(&mut self) {
        self.term.substitute();
    }
    fn newline(&mut self) {
        self.line_scroll();
        self.term.newline();
    }
    fn set_horizontal_tabstop(&mut self) {
        self.term.set_horizontal_tabstop();
    }
    fn scroll_up(&mut self, v0: usize) {
        let (top, bottom) = self.region();
        self.scroll(top, bottom, v0.min((bottom - top + 1) as usize) as i32);
        self.term.scroll_up(v0);
    }
    fn scroll_down(&mut self, v0: usize) {
        let (top, bottom) = self.region();
        self.scroll(top, bottom, -(v0.min((bottom - top + 1) as usize) as i32));
        self.term.scroll_down(v0);
    }
    fn insert_blank_lines(&mut self, v0: usize) {
        let (_, bottom) = self.region();
        let top = self.term.grid().cursor.point.line.0;
        self.scroll(
            top,
            bottom,
            -(v0.min((bottom - top + 1).max(0) as usize) as i32),
        );
        self.term.insert_blank_lines(v0);
    }
    fn delete_lines(&mut self, v0: usize) {
        let (_, bottom) = self.region();
        let top = self.term.grid().cursor.point.line.0;
        self.scroll(
            top,
            bottom,
            v0.min((bottom - top + 1).max(0) as usize) as i32,
        );
        self.term.delete_lines(v0);
    }
    fn erase_chars(&mut self, v0: usize) {
        self.term.erase_chars(v0);
    }
    fn delete_chars(&mut self, v0: usize) {
        self.term.delete_chars(v0);
    }
    fn move_backward_tabs(&mut self, v0: u16) {
        self.term.move_backward_tabs(v0);
    }
    fn move_forward_tabs(&mut self, v0: u16) {
        self.term.move_forward_tabs(v0);
    }
    fn save_cursor_position(&mut self) {
        self.term.save_cursor_position();
    }
    fn restore_cursor_position(&mut self) {
        self.term.restore_cursor_position();
    }
    fn clear_line(&mut self, v0: LineClearMode) {
        self.term.clear_line(v0);
    }
    fn clear_screen(&mut self, v0: ClearMode) {
        self.term.clear_screen(v0);
    }
    fn clear_tabs(&mut self, v0: TabulationClearMode) {
        self.term.clear_tabs(v0);
    }
    fn set_tabs(&mut self, v0: u16) {
        self.term.set_tabs(v0);
    }
    fn reset_state(&mut self) {
        *self.graphics = Graphics::default();
        self.term.reset_state();
    }
    fn reverse_index(&mut self) {
        let (top, bottom) = self.region();
        if self.term.grid().cursor.point.line.0 == top {
            self.scroll(top, bottom, -1);
        }
        self.term.reverse_index();
    }
    fn terminal_attribute(&mut self, v0: Attr) {
        self.term.terminal_attribute(v0);
    }
    fn set_mode(&mut self, v0: Mode) {
        self.term.set_mode(v0);
    }
    fn unset_mode(&mut self, v0: Mode) {
        self.term.unset_mode(v0);
    }
    fn report_mode(&mut self, v0: Mode) {
        self.term.report_mode(v0);
    }
    fn set_private_mode(&mut self, v0: PrivateMode) {
        if v0 == PrivateMode::Unknown(80) {
            self.graphics.sixel_display_mode = true;
            return;
        }
        let previous = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.term.set_private_mode(v0);
        if !previous && self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.graphics.clear_alternate();
        }
    }
    fn unset_private_mode(&mut self, v0: PrivateMode) {
        if v0 == PrivateMode::Unknown(80) {
            self.graphics.sixel_display_mode = false;
            return;
        }
        let previous = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.term.unset_private_mode(v0);
        if previous && !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.graphics.clear_alternate();
        }
    }
    fn report_private_mode(&mut self, v0: PrivateMode) {
        self.term.report_private_mode(v0);
    }
    fn set_scrolling_region(&mut self, v0: usize, v1: Option<usize>) {
        let bottom = v1.unwrap_or(self.term.screen_lines());
        if v0 < bottom {
            self.graphics.region = Some((
                v0.saturating_sub(1) as i32,
                bottom.saturating_sub(1).min(self.term.screen_lines() - 1) as i32,
            ));
        }
        self.term.set_scrolling_region(v0, v1);
    }
    fn set_keypad_application_mode(&mut self) {
        self.term.set_keypad_application_mode();
    }
    fn unset_keypad_application_mode(&mut self) {
        self.term.unset_keypad_application_mode();
    }
    fn set_active_charset(&mut self, v0: CharsetIndex) {
        self.term.set_active_charset(v0);
    }
    fn configure_charset(&mut self, v0: CharsetIndex, v1: StandardCharset) {
        self.term.configure_charset(v0, v1);
    }
    fn set_color(&mut self, v0: usize, v1: Rgb) {
        self.term.set_color(v0, v1);
    }
    fn dynamic_color_sequence(&mut self, v0: String, v1: usize, v2: &str) {
        self.term.dynamic_color_sequence(v0, v1, v2);
    }
    fn reset_color(&mut self, v0: usize) {
        self.term.reset_color(v0);
    }
    fn clipboard_store(&mut self, v0: u8, v1: &[u8]) {
        self.term.clipboard_store(v0, v1);
    }
    fn clipboard_load(&mut self, v0: u8, v1: &str) {
        self.term.clipboard_load(v0, v1);
    }
    fn decaln(&mut self) {
        self.term.decaln();
    }
    fn push_title(&mut self) {
        self.term.push_title();
    }
    fn pop_title(&mut self) {
        self.term.pop_title();
    }
    fn text_area_size_pixels(&mut self) {
        self.term.text_area_size_pixels();
    }
    fn text_area_size_chars(&mut self) {
        self.term.text_area_size_chars();
    }
    fn set_hyperlink(&mut self, v0: Option<Hyperlink>) {
        self.term.set_hyperlink(v0);
    }
    fn set_mouse_cursor_icon(&mut self, v0: CursorIcon) {
        self.term.set_mouse_cursor_icon(v0);
    }
    fn report_keyboard_mode(&mut self) {
        self.term.report_keyboard_mode();
    }
    fn push_keyboard_mode(&mut self, v0: KeyboardModes) {
        self.term.push_keyboard_mode(v0);
    }
    fn pop_keyboard_modes(&mut self, v0: u16) {
        self.term.pop_keyboard_modes(v0);
    }
    fn set_keyboard_mode(&mut self, v0: KeyboardModes, v1: KeyboardModesApplyBehavior) {
        self.term.set_keyboard_mode(v0, v1);
    }
    fn set_modify_other_keys(&mut self, v0: ModifyOtherKeys) {
        self.term.set_modify_other_keys(v0);
    }
    fn report_modify_other_keys(&mut self) {
        self.term.report_modify_other_keys();
    }
    fn set_scp(&mut self, v0: ScpCharPath, v1: ScpUpdateMode) {
        self.term.set_scp(v0, v1);
    }
}
