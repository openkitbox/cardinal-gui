//! Status page: the single view of the application. Its five views (概览,
//! 账号, 路由, 请求, 设置) switch via a top tab bar. Tab
//! selection lives in `AppState::status_tab` (single source of truth), so
//! the tab bar highlight and this page stay in sync without cross-entity
//! calls; this container only renders whichever child is active.

use gpui::{AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, Window};
use gpui_kit::component::tab::{Tab, TabBar};

use crate::app::{AppState, StatusTab};
use crate::pages::{
    ActionPage, ProvidersPage, RequestsPage, RoutesPage, SettingsPage, UsagePage, UsersPage,
};

/// Container for the four status views. Owns their entities (created eagerly,
/// like the other routes) and renders the active one under the tab bar; each
/// child keeps its own load state and kicks off its first fetch on first
/// render.
pub struct StatusPage {
    state: Entity<AppState>,
    providers: Entity<ProvidersPage>,
    routes: Entity<RoutesPage>,
    requests: Entity<RequestsPage>,
    usage: Entity<UsagePage>,
    users: Entity<UsersPage>,
    settings: Entity<SettingsPage>,
    /// The status tab currently showing; a change activates a child: its
    /// snapshot reloads silently (`begin_load` keeps the old data on screen —
    /// no flicker) so a freshly shown tab is never stale.
    active_tab: Option<StatusTab>,
}

impl StatusPage {
    pub(crate) fn new(
        state: Entity<AppState>,
        feed: Entity<crate::activity::ActivityFeed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-render on AppState changes; a tab activation silently refreshes
        // the newly shown child.
        cx.observe(&state, |page, state, cx| {
            let active = Some(state.read(cx).status_tab);
            if page.active_tab != active {
                page.active_tab = active;
                if let Some(tab) = active {
                    page.refresh_child(tab, cx);
                }
                cx.notify();
            }
        })
        .detach();
        Self {
            providers: cx.new(|cx| ProvidersPage::new(feed.clone(), cx)),
            routes: cx.new(|cx| RoutesPage::new(feed.clone(), cx)),
            requests: cx.new(|cx| RequestsPage::new(feed.clone(), cx)),
            usage: cx.new(|cx| UsagePage::new(feed.clone(), window, cx)),
            users: cx.new(|cx| UsersPage::new(feed, window, cx)),
            settings: cx.new(|cx| SettingsPage::new(window, cx)),
            state,
            active_tab: None,
        }
    }

    /// Silently refresh the newly shown tab (switching tabs still re-reads).
    /// Construction itself does NOT load: each page opens on the activity
    /// stream's connect frame (the pushed bootstrap or its resync). Settings
    /// is read-only with no reload — its data comes from the startup probe.
    fn refresh_child(&mut self, tab: StatusTab, cx: &mut Context<Self>) {
        match tab {
            StatusTab::Providers => self.providers.update(cx, |page, cx| page.reload(cx)),
            StatusTab::Routes => self.routes.update(cx, |page, cx| page.reload(cx)),
            StatusTab::Requests => self.requests.update(cx, |page, cx| page.reload(cx)),
            StatusTab::Usage => self.usage.update(cx, |page, cx| page.reload(cx)),
            StatusTab::Users => self.users.update(cx, |page, cx| page.reload(cx)),
            StatusTab::Settings => {}
        }
    }
}

impl Render for StatusPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.state.read(cx).status_tab;
        let state = self.state.clone();
        // Per-tab actions render right-aligned in the tab row; each page
        // decides what it offers. Settings is read-only — no actions.
        let actions = match tab {
            StatusTab::Providers => self.providers.update(cx, |page, cx| page.actions(cx)),
            StatusTab::Routes => self.routes.update(cx, |page, cx| page.actions(cx)),
            StatusTab::Requests => self.requests.update(cx, |page, cx| page.actions(cx)),
            StatusTab::Usage => self.usage.update(cx, |page, cx| page.actions(cx)),
            StatusTab::Users => self.users.update(cx, |page, cx| page.actions(cx)),
            StatusTab::Settings => Vec::new(),
        };
        let content = match tab {
            StatusTab::Providers => self.providers.clone().into_any_element(),
            StatusTab::Routes => self.routes.clone().into_any_element(),
            StatusTab::Requests => self.requests.clone().into_any_element(),
            StatusTab::Usage => self.usage.clone().into_any_element(),
            StatusTab::Users => self.users.clone().into_any_element(),
            StatusTab::Settings => self.settings.clone().into_any_element(),
        };

        gpui::div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(
                gpui::div()
                    .px_6()
                    .pt_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        TabBar::new("status-tabs")
                            .underline()
                            .selected_index(tab as usize)
                            .on_click(move |index, _, cx| {
                                if let Some(next) = StatusTab::ALL.get(*index).copied() {
                                    state.update(cx, |state, cx| state.set_status_tab(next, cx));
                                }
                            })
                            .children(StatusTab::ALL.map(|tab| Tab::new().label(tab.label()))),
                    )
                    .child(gpui::div().flex().items_center().gap_2().children(actions)),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(content),
            )
    }
}
