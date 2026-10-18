//! App shell and shared state: the root view with a tab-bar content area and
//! a bottom status bar. [`AppState`] holds the active tab as the single
//! source of truth. [`App`] owns the status page entity.

use gpui::{
    AppContext as _, Context, Div, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
    div, prelude::*,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::Button;

use crate::activity::ActivityFeed;
use crate::pages::StatusPage;

/// The status page's tab: which sub-page the tab bar shows. Kept in
/// `AppState` as the single source of truth so the tab bar and page content
/// stay in sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusTab {
    Usage,
    Providers,
    Routes,
    Requests,
    Users,
    Settings,
}

impl StatusTab {
    /// All tabs, in tab-bar order; `as usize` indexes into this.
    pub(crate) const ALL: [StatusTab; 6] = [
        StatusTab::Usage,
        StatusTab::Providers,
        StatusTab::Routes,
        StatusTab::Requests,
        StatusTab::Users,
        StatusTab::Settings,
    ];

    /// Tab label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StatusTab::Usage => "概览",
            StatusTab::Providers => "账号",
            StatusTab::Routes => "路由",
            StatusTab::Requests => "请求",
            StatusTab::Users => "用户",
            StatusTab::Settings => "设置",
        }
    }
}

/// The shared application state: the active status tab is the single source
/// of truth for which page the tab bar shows.
pub struct AppState {
    pub(crate) status_tab: StatusTab,
}

impl AppState {
    /// Set the status page's active tab.
    pub(crate) fn set_status_tab(&mut self, tab: StatusTab, cx: &mut Context<Self>) {
        if self.status_tab != tab {
            self.status_tab = tab;
            cx.notify();
        }
    }
}

/// The root view: tab-bar content area with a bottom status bar.
pub struct App {
    status_page: Entity<StatusPage>,
    feed: Entity<ActivityFeed>,
    focus_handle: FocusHandle,
    /// Gateway relation, resolved once startup completes (None = probing).
    relay: Option<Result<crate::runtime::RelayMode, String>>,
    /// The last RESOLVED gateway relation (survives the probing state, where
    /// `relay` is None): lets a retry that lands Connected detect the
    /// Unreachable→Connected heal and re-fire the one-shot loads.
    last_relay: Option<crate::runtime::RelayMode>,
}

impl App {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_cx| AppState {
            status_tab: StatusTab::Usage,
        });

        // The shared activity feed: one entity, one gateway SSE subscription,
        // every page renders it. The subscription is taken once — this is that
        // once.
        let feed = cx.new(ActivityFeed::new);

        let status_page = cx.new(|cx| StatusPage::new(state.clone(), feed.clone(), window, cx));

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        // One-shot: the gateway relation lands once the startup probe resolves.
        Self::watch_relay_spawn(cx);

        Self {
            status_page,
            feed,
            focus_handle,
            relay: None,
            last_relay: None,
        }
    }

    /// The status-bar 重试 (shown while the gateway is unreachable): re-run the
    /// startup probe; the bar shows 连接中… until it resolves. Pages reload on
    /// their own next action.
    fn retry_gateway(&mut self, cx: &mut Context<Self>) {
        cx.global::<crate::runtime::GatewayRuntime>().reconnect();
        self.relay = None;
        Self::watch_relay_spawn(cx);
        cx.notify();
    }

    /// Await the current startup probe and land its outcome in `self.relay`.
    /// An Unreachable→Connected transition (the status-bar retry succeeding)
    /// additionally heals the one-shot loads: the activity feed gets one
    /// synthetic persisted bump so observing pages reload.
    fn watch_relay_spawn(cx: &mut Context<Self>) {
        let relay_mode = cx.global::<crate::runtime::GatewayRuntime>().relay_mode();
        cx.spawn(async move |this, cx| {
            let mode = relay_mode.await;
            this.update(cx, |app, cx| {
                let healed = matches!(app.last_relay, Some(crate::runtime::RelayMode::Unreachable))
                    && matches!(mode, Ok(crate::runtime::RelayMode::Connected(_)));
                app.last_relay = mode.clone().ok();
                app.relay = Some(mode);
                if healed {
                    app.feed.update(cx, |feed, cx| feed.notify_persisted(cx));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Focusable for App {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for App {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(gpui::px(13.))
            .track_focus(&self.focus_handle(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(self.status_page.clone())
                    .child(status_bar(&self.relay, cx)),
            )
    }
}

/// Bottom status bar of the content area. Left: the gateway relay — dot color
/// carries the state, the text is the listen address (details in the
/// tooltip); an unreachable gateway gets a 重试 button that re-runs the
/// startup probe. Right: the effective config file, build version.
fn status_bar(
    relay: &Option<Result<crate::runtime::RelayMode, String>>,
    cx: &mut Context<App>,
) -> Div {
    use crate::runtime::RelayMode;
    // Mid-session death: the one-shot probe resolved Connected long ago, but
    // the last admin call failed at connect — render as unreachable.
    let link_down = cx.global::<crate::runtime::GatewayRuntime>().link_down();
    let (dot, text, tip): (_, String, String) = match relay {
        None => (
            cx.theme().muted_foreground,
            "连接中…".to_string(),
            "网关连接探测中".to_string(),
        ),
        Some(Ok(RelayMode::Connected(addr))) if !link_down => (
            cx.theme().success,
            addr.clone(),
            "gateway 独立进程（cardinal-gateway）".to_string(),
        ),
        Some(Ok(_)) => (
            cx.theme().danger,
            "未运行".to_string(),
            gateway::http_client::UNREACHABLE_MESSAGE.to_string(),
        ),
        Some(Err(error)) => (cx.theme().danger, "不可用".to_string(), error.clone()),
    };
    let mut gateway = gpui::div()
        .id("status-gateway")
        .flex()
        .items_center()
        .gap_1()
        .child(
            gpui::div()
                .text_color(cx.theme().muted_foreground)
                .child("网关"),
        )
        .child(gpui::div().text_color(dot).child("●"))
        .child(gpui::div().text_color(cx.theme().foreground).child(text))
        .tooltip(move |_, cx| cx.new(|_| StatusTip { text: tip.clone() }).into());
    if link_down || matches!(relay, Some(Ok(RelayMode::Unreachable))) {
        gateway = gateway.child(
            Button::new("gateway-retry")
                .label("重试")
                .outline()
                .compact()
                .on_click(cx.listener(|this, _, _, cx| this.retry_gateway(cx))),
        );
    }
    gpui::div()
        .flex()
        .items_center()
        .justify_between()
        .h(gpui::px(26.))
        .px_3()
        .border_t_1()
        .border_color(cx.theme().border)
        .text_size(gpui::px(11.))
        .child(gpui::div().flex().items_center().gap_3().child(gateway))
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    gpui::div()
                        .text_color(cx.theme().muted_foreground)
                        .child("~/.ox/config.toml"),
                )
                .child(
                    gpui::div()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                ),
        )
}

/// Tooltip body for the status-bar dots.
pub(crate) struct StatusTip {
    pub(crate) text: String,
}

impl Render for StatusTip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .px_2()
            .py_1()
            .text_size(gpui::px(12.))
            .text_color(cx.theme().foreground)
            .child(self.text.clone())
    }
}
