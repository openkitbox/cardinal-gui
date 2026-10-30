//! The toggle chip shared by the pickers (model whitelist, group membership).

use gpui::{App, Div, ElementId, InteractiveElement, ParentElement, Styled, px};
use gpui_kit::component::ActiveTheme;

/// A bordered toggle chip: accent-filled when selected, hover-highlighted
/// otherwise. Shared by the model whitelist and the group pickers.
pub(super) fn selectable_chip(
    text: String,
    selected: bool,
    id: ElementId,
    cx: &App,
) -> gpui::Stateful<Div> {
    let chip = gpui::div()
        .id(id)
        .cursor_pointer()
        .border_1()
        .rounded_md()
        .px_2()
        .py_0p5()
        .text_size(px(12.))
        .child(text);
    if selected {
        chip.border_color(cx.theme().accent)
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
    } else {
        chip.border_color(cx.theme().border)
            .text_color(cx.theme().foreground)
            .hover(|this| this.bg(cx.theme().accent))
    }
}
