//! Non-chat pages behind the sidebar routes: Providers, Routes, Health,
//! Requests, Usage, Settings. Dashboard pages read live data from the
//! standalone gateway over HTTP via `GatewayRuntime::admin_call` (see
//! `crate::runtime`); each is a self-contained entity holding a
//! [`LoadState`]. Providers, Routes and Health are read-write: mutations go
//! through `GatewayHttpClient`'s write methods and end in a reload.

mod feed;
mod gateway_keys;
mod ledger;
mod providers;
mod requests;
mod routes;
mod settings;
mod status;
mod usage;
mod usage_trend;
mod user_groups;
mod users;

pub use gateway_keys::GatewayKeysPage;
pub use providers::ProvidersPage;
pub use requests::RequestsPage;
pub use routes::RoutesPage;
pub use settings::SettingsPage;
pub use status::StatusPage;
pub use usage::UsagePage;
pub use users::UsersPage;

use gpui::{
    AnyWindowHandle, App, ClickEvent, Context, Div, InteractiveElement, IntoElement, ParentElement,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme, Icon, Sizable, WindowExt};

/// Async load lifecycle shared by the dashboard pages: first render kicks off
/// the load, Refresh re-runs it; there are no polling timers.
pub(crate) enum LoadState<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

/// In-app notification ids: pushing with the same id replaces the previous
/// toast, so consecutive results of the same kind never stack up.
struct SuccessNoticeId;
struct ErrorNoticeId;

/// Push a success toast into the captured window; a closed window is ignored.
pub(crate) fn notify_success(
    handle: AnyWindowHandle,
    cx: &mut App,
    message: impl Into<SharedString>,
) {
    let note = Notification::success(message).id::<SuccessNoticeId>();
    let _ = handle.update(cx, |_, window, cx| window.push_notification(note, cx));
}

/// Push an error toast into the captured window; a closed window is ignored.
pub(crate) fn notify_error(
    handle: AnyWindowHandle,
    cx: &mut App,
    message: impl Into<SharedString>,
) {
    let note = Notification::error(message).id::<ErrorNoticeId>();
    let _ = handle.update(cx, |_, window, cx| window.push_notification(note, cx));
}

/// A refresh keeps the last snapshot visible (no flicker, no editor focus
/// loss); only the first load shows the spinner. Call at the top of reload.
pub(crate) fn begin_load<T>(state: &mut LoadState<T>) {
    if !matches!(state, LoadState::Loaded(_)) {
        *state = LoadState::Loading;
    }
}

/// Shared page chrome: padded, vertically scrollable column with a title and
/// subtitle. An empty title renders no header at all. Scrolling requires a
/// stateful (id'd) div.
pub(crate) fn page_frame(title: &'static str, subtitle: &'static str, cx: &App) -> Stateful<Div> {
    page_frame_impl(title, subtitle, cx)
}

fn page_frame_impl(title: &'static str, subtitle: &'static str, cx: &App) -> Stateful<Div> {
    let frame = gpui::div()
        .id("page-frame")
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap_4()
        .p_6()
        .overflow_y_scroll();
    // An empty title means the page supplies its own header (or none); skip
    // the chrome entirely.
    if title.is_empty() {
        return frame;
    }
    frame.child(
        gpui::div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                gpui::div()
                    .text_size(px(18.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(title),
            )
            .child(
                gpui::div()
                    .text_color(cx.theme().muted_foreground)
                    .child(subtitle),
            ),
    )
}

/// Tab-row Refresh button (rendered by the Status container's tab row).
pub(crate) fn refresh_button(
    id: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .icon(Icon::new(IconName::RefreshCw).small())
        .label("刷新")
        .outline()
        .compact()
        .on_click(on_click)
}

/// Bordered card surface used by the dashboard-style pages.
pub(crate) fn card(cx: &App) -> Div {
    gpui::div()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().popover)
        .p_4()
}

/// Spinner card shown while a load is in flight.
pub(crate) fn loading_card(cx: &App) -> Div {
    card(cx)
        .flex_row()
        .items_center()
        .gap_2()
        .child(Spinner::new().color(cx.theme().muted_foreground))
        .child(
            gpui::div()
                .text_color(cx.theme().muted_foreground)
                .child("加载中…"),
        )
}

/// Error card: a failed startup or fetch, with the message.
pub(crate) fn error_card(message: &str, cx: &App) -> Div {
    card(cx)
        .child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().danger)
                .child("加载失败"),
        )
        .child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(message.to_string()),
        )
}

/// Empty-state card: a title plus a dimmed hint.
pub(crate) fn empty_card(title: &'static str, hint: impl Into<String>, cx: &App) -> Div {
    card(cx)
        .child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().foreground)
                .child(title),
        )
        .child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(hint.into()),
        )
}

/// Segmented control backed by GPUI Kit's animated tab indicator.
pub(crate) fn segmented<P: 'static, T: Copy + Eq + 'static>(
    id_prefix: &'static str,
    options: &[(T, &'static str)],
    current: T,
    cx: &mut Context<P>,
    on_pick: impl Fn(&mut P, T, &mut Context<P>) + Copy + 'static,
) -> impl IntoElement {
    let selected_index = options
        .iter()
        .position(|(value, _)| *value == current)
        .unwrap_or(0);
    let on_pick = std::rc::Rc::new(on_pick);
    TabBar::new(id_prefix)
        .segmented()
        .selected_index(selected_index)
        .children(options.iter().map(|&(value, label)| {
            let on_pick = std::rc::Rc::clone(&on_pick);
            Tab::new()
                .label(label)
                .on_click(cx.listener(move |page, _, _, cx| on_pick(page, value, cx)))
        }))
}

/// Fixed-width dimmed label at the start of an editor row.
pub(crate) fn field_label(label: &'static str, cx: &App) -> Div {
    gpui::div()
        .w(px(72.))
        .flex_shrink_0()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// One labeled editor row; `width` sizes the input, `None` fills the row.
pub(crate) fn editor_row(label: &'static str, input: Input, width: Option<f32>, cx: &App) -> Div {
    let row = gpui::div()
        .flex()
        .items_center()
        .gap_2()
        .child(field_label(label, cx));
    match width {
        Some(width) => row.child(input.w(px(width))),
        None => row.child(gpui::div().flex_1().child(input.w_full())),
    }
}

/// A ghost icon button with a tooltip. The icon must be embedded (default
/// set or `assets::VENDORED`), else it renders blank.
pub(crate) fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    tooltip: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .icon(Icon::new(icon).small())
        .ghost()
        .compact()
        .tooltip(tooltip)
        .on_click(on_click)
}

/// `YYYY-MM-DD HH:MM` in the GUI display timezone.
pub(crate) fn display_time(at: jiff::Timestamp) -> String {
    crate::time::format(at, crate::ui_tz::display_tz(), "%Y-%m-%d %H:%M")
}

/// Small inline error text under an editor field or form.
pub(crate) fn inline_error(message: impl Into<gpui::SharedString>, cx: &App) -> Div {
    gpui::div()
        .text_size(px(12.))
        .text_color(cx.theme().danger)
        .child(message.into())
}
