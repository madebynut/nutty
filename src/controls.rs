//! Shared flat input surfaces and pointer affordances for native controls.

use gpui_kit::component::{
    Theme,
    button::Button,
    input::{Input, InputState, Textarea, TextareaState},
    select::{SearchableVec, Select, SelectState},
};
use gpui_kit::{App, Context, ElementId, Entity, SharedString, Styled, Window, px};

pub(crate) type Choices = SearchableVec<SharedString>;

pub(crate) fn select_state(
    items: Vec<SharedString>,
    selected: Option<gpui_kit::component::IndexPath>,
    window: &mut Window,
    cx: &mut Context<SelectState<Choices>>,
) -> SelectState<Choices> {
    SelectState::new(Choices::new(items), selected, window, cx)
}

pub(crate) fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id).cursor_pointer().shadow_none()
}

pub(crate) fn input(state: &Entity<InputState>, cx: &App) -> Input {
    let theme = cx.global::<Theme>();
    Input::new(state)
        .min_h(theme.font_size * 1.4 + px(8.))
        .rounded(px(6.))
        .bg(theme.secondary)
        .border_color(theme.border)
        .text_size(theme.font_size)
        .shadow_none()
}

pub(crate) fn textarea(state: &Entity<TextareaState>, cx: &App) -> Textarea {
    let theme = cx.global::<Theme>();
    Textarea::new(state)
        .rounded(px(6.))
        .bg(theme.secondary)
        .border_color(theme.border)
        .text_size(theme.font_size)
        .shadow_none()
}

pub(crate) fn select(state: &Entity<SelectState<Choices>>, cx: &App) -> Select<Choices> {
    let theme = cx.global::<Theme>();
    Select::new(state)
        .search_placeholder(crate::i18n::text("搜索…"))
        .min_h(theme.font_size * 1.4 + px(8.))
        .cursor_pointer()
        .rounded(px(6.))
        .bg(theme.secondary)
        .border_color(theme.border)
        .text_size(theme.font_size)
        .shadow_none()
}
