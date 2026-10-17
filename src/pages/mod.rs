//! Non-chat pages behind the sidebar routes: Providers, Routes, Health,
//! Requests, Usage, Settings. Dashboard pages read live data from the
//! standalone gateway over HTTP via `GatewayRuntime::admin_call` (see
//! `crate::runtime`); each is a self-contained entity holding a
//! [`LoadState`]. Providers, Routes and Health are read-write: mutations go
//! through `GatewayHttpClient`'s write methods and end in a reload.

mod feed;
mod providers;
mod requests;
mod routes;
mod settings;
mod status;
mod usage;
mod usage_trend;
mod users;

pub use providers::ProvidersPage;
pub use requests::RequestsPage;
pub use routes::RoutesPage;
pub use settings::SettingsPage;
pub use status::StatusPage;
pub use usage::UsagePage;
pub use users::UsersPage;

use gpui::{
    App, ClickEvent, Context, Div, InteractiveElement, ParentElement, Stateful,
    StatefulInteractiveElement, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Icon, Sizable};

use gateway::http_client::GatewayHttpClient;

use crate::runtime::GatewayRuntime;

/// Async load lifecycle shared by the dashboard pages: first render kicks off
/// the load, Refresh re-runs it; there are no polling timers.
pub(crate) enum LoadState<T> {
    Loading,
    Loaded(T),
    Failed(String),
}

/// A dashboard page that runs admin writes: row-level failures surface above
/// the page's content via an action-error slot; a successful write triggers a
/// reload. (`Sized` is required by `Context<Self>`; the trait is only used
/// generically.)
pub(crate) trait ActionPage: Sized + 'static {
    fn action_error_slot(&mut self) -> &mut Option<String>;
    fn reload(&mut self, cx: &mut Context<Self>);
}

/// Run one write against the gateway's HTTP admin API on an [`ActionPage`]:
/// on success clear the error and reload; on failure show the message.
pub(crate) fn run_write<P, F, Fut>(cx: &mut Context<P>, f: F)
where
    P: ActionPage,
    F: FnOnce(GatewayHttpClient) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), gateway::http_client::ClientError>>
        + Send
        + 'static,
{
    let call = cx.global::<GatewayRuntime>().admin_call(f);
    cx.spawn(async move |this, cx| {
        let result = call.await;
        this.update(cx, |page, cx| match result {
            Ok(()) => {
                *page.action_error_slot() = None;
                page.reload(cx);
            }
            Err(error) => {
                *page.action_error_slot() = Some(error);
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
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

/// Page-level write failure (toggle/delete/reorder/cooldown clear), shown
/// above the list; editor-level validation errors stay in their cards.
pub(crate) fn action_error_card(message: &str, cx: &App) -> Div {
    card(cx)
        .child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().danger)
                .child("操作失败"),
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
