//! Users page: gateway API key management. Create keys, enable/disable them.
//! Keys are stored as SHA-256 hashes; the plaintext is shown exactly once at
//! creation time and cannot be retrieved later.

use gpui::{
    AppContext, Context, Div, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, Sizable};

use gateway::store::GatewayKeyRow;

use crate::pages::{
    ActionPage, LoadState, begin_load, empty_card, error_card, loading_card, page_frame,
    refresh_button, run_write,
};
use crate::runtime::GatewayRuntime;

/// Gateway API key management page.
pub struct UsersPage {
    state: LoadState<Vec<GatewayKeyRow>>,
    /// Action error slot (write failures surface above the content).
    action_error: Option<String>,
    /// Label input for the "create key" form.
    label_input: Entity<InputState>,
    /// Whether the create-key form is visible (toggled by the "新增 key" button).
    show_create_form: bool,
    /// Last seen bootstrap revision — the page renders the pushed document's
    /// gateway keys; a new document (connect/reconnect) re-applies.
    seen_bootstrap: u64,
    /// Last seen resync revision — a stream-continuity gap falls back to a
    /// pull reload.
    seen_resync: u64,
}

impl UsersPage {
    pub fn new(
        feed: gpui::Entity<crate::activity::ActivityFeed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("label（标识 key 的归属）"));
        let mut page = Self {
            state: LoadState::Loading,
            action_error: None,
            label_input,
            show_create_form: false,
            seen_bootstrap: feed.read(cx).bootstrap_rev(),
            seen_resync: feed.read(cx).resync(),
        };
        // Render the pushed bootstrap document's gateway keys — opening the
        // client costs no gateway-keys request. A `resync` gap (stream
        // continuity lost) falls back to the pull reload.
        cx.observe(&feed, |page, feed, cx| {
            let bootstrap_rev = feed.read(cx).bootstrap_rev();
            if bootstrap_rev != page.seen_bootstrap {
                page.seen_bootstrap = bootstrap_rev;
                if let Some(document) = feed.read(cx).bootstrap() {
                    page.state = LoadState::Loaded(document.gateway_keys.clone());
                }
                cx.notify();
            }
            let resync = feed.read(cx).resync();
            if resync != page.seen_resync {
                page.seen_resync = resync;
                page.reload(cx);
            }
        })
        .detach();
        // A document that landed before this page existed still applies.
        if let Some(document) = feed.read(cx).bootstrap() {
            page.state = LoadState::Loaded(document.gateway_keys.clone());
        }
        page
    }

    fn create_key(&mut self, cx: &mut Context<Self>) {
        let label = self.label_input.read(cx).value().trim().to_string();
        if label.is_empty() {
            self.action_error = Some("请输入 label".to_string());
            cx.notify();
            return;
        }
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.create_gateway_key(&label).await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| match result {
                Ok(_key) => {
                    page.action_error = None;
                    page.show_create_form = false;
                    page.reload(cx);
                }
                Err(error) => {
                    page.action_error = Some(error);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_key(&mut self, id: i64, enabled: bool, cx: &mut Context<Self>) {
        run_write(cx, move |admin| async move {
            admin.set_gateway_key_enabled(id, enabled).await
        });
    }
}

impl ActionPage for UsersPage {
    fn action_error_slot(&mut self) -> &mut Option<String> {
        &mut self.action_error
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        begin_load(&mut self.state);
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move { admin.gateway_keys().await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.state = match result {
                    Ok(keys) => LoadState::Loaded(keys),
                    Err(error) => LoadState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl UsersPage {
    /// Tab-bar actions (right-aligned): add-key button + refresh button.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "users-refresh",
            cx.listener(|this, _, _, cx| this.reload(cx)),
        );
        match &self.state {
            LoadState::Loaded(_) => vec![
                Button::new("key-create")
                    .icon(Icon::new(IconName::Plus).small())
                    .label("新增 key")
                    .outline()
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_create_form(window, cx);
                    }))
                    .into_any_element(),
                refresh.into_any_element(),
            ],
            _ => vec![refresh.into_any_element()],
        }
    }

    /// Show the create-key form and focus the label input.
    fn open_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_create_form = true;
        let focus = self.label_input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Hide the create-key form (cancel or after successful creation).
    fn close_create_form(&mut self, cx: &mut Context<Self>) {
        self.show_create_form = false;
        cx.notify();
    }
}

impl Render for UsersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = gpui::div().flex().flex_col().gap_4();

        // Action error slot.
        if let Some(error) = &self.action_error {
            content = content.child(
                gpui::div()
                    .p_3()
                    .rounded_md()
                    .bg(cx.theme().danger.opacity(0.1))
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }

        // Create form (visible only when toggled by the "新增 key" button).
        if self.show_create_form {
            content = content.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(gpui::div().flex_1().child(Input::new(&self.label_input)))
                    .child(
                        Button::new("create-key")
                            .primary()
                            .compact()
                            .label("创建")
                            .on_click(cx.listener(|this, _, _, cx| this.create_key(cx))),
                    )
                    .child(
                        Button::new("cancel-create")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| this.close_create_form(cx))),
                    ),
            );
        }

        // Key list.
        match &self.state {
            LoadState::Loading => {
                content = content.child(loading_card(cx));
            }
            LoadState::Failed(error) => {
                content = content.child(error_card(error, cx));
            }
            LoadState::Loaded(keys) => {
                if keys.is_empty() {
                    content = content.child(empty_card("暂无 key", "", cx));
                } else {
                    content = content.child(self.key_table(keys, cx));
                }
            }
        }

        page_frame("", "", cx).child(content)
    }
}

impl UsersPage {
    /// The key table: label, key, status, created, last used, actions.
    fn key_table(&self, keys: &[GatewayKeyRow], cx: &mut Context<Self>) -> Div {
        let mut table = gpui::div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border);

        // Header.
        table = table.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_4()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(gpui::div().w(px(140.)).child("Label"))
                .child(gpui::div().flex_1().child("Key"))
                .child(gpui::div().w(px(60.)).child("状态"))
                .child(gpui::div().w(px(140.)).child("创建时间"))
                .child(gpui::div().w(px(140.)).child("最后使用"))
                .child(gpui::div().w(px(120.)).child("操作")),
        );

        // Rows.
        for (index, key) in keys.iter().enumerate() {
            table = table.child(self.key_row(key, index, cx));
        }

        table
    }

    fn key_row(&self, key: &GatewayKeyRow, index: usize, cx: &mut Context<Self>) -> Div {
        // Status: colored text, no background (reference: requests page HTTP
        // status).
        let (status_text, status_color) = if key.enabled {
            ("启用", gpui::green())
        } else {
            ("暂停", gpui::red())
        };

        let toggle_label = if key.enabled { "暂停" } else { "启用" };
        let toggle_enabled = !key.enabled;
        let key_id = key.id;
        let key_copy = key.key.clone();

        gpui::div()
            .flex()
            .items_center()
            .gap_4()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .child(
                gpui::div()
                    .w(px(140.))
                    .text_color(cx.theme().foreground)
                    .child(key.label.clone()),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .font_family("monospace")
                    .text_color(cx.theme().muted_foreground)
                    .child(key.key.clone()),
            )
            .child(
                gpui::div()
                    .w(px(60.))
                    .text_color(status_color)
                    .child(status_text),
            )
            .child(
                gpui::div()
                    .w(px(140.))
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        key.created_at
                            .with_timezone(&crate::ui_tz::display_tz())
                            .format("%Y-%m-%d %H:%M")
                            .to_string(),
                    ),
            )
            .child(
                gpui::div()
                    .w(px(140.))
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        key.last_used_at
                            .map(|t| {
                                t.with_timezone(&crate::ui_tz::display_tz())
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_else(|| "—".to_string()),
                    ),
            )
            .child(
                gpui::div()
                    .w(px(120.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        gpui::div()
                            .id(("copy-key", index))
                            .child(
                                Button::new(("copy-key-btn", index))
                                    .icon(Icon::new(IconName::Copy).small())
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            key_copy.clone(),
                                        ));
                                    })),
                            )
                            .tooltip(move |_, cx| {
                                cx.new(|_| crate::app::StatusTip {
                                    text: "复制".to_string(),
                                })
                                .into()
                            }),
                    )
                    .child(
                        gpui::div()
                            .id(("toggle-key", index))
                            .child(
                                Button::new(("toggle-key-btn", index))
                                    .icon(
                                        Icon::new(if key.enabled {
                                            IconName::Pause
                                        } else {
                                            IconName::Play
                                        })
                                        .small(),
                                    )
                                    .ghost()
                                    .compact()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_key(key_id, toggle_enabled, cx)
                                    })),
                            )
                            .tooltip(move |_, cx| {
                                cx.new(|_| crate::app::StatusTip {
                                    text: toggle_label.to_string(),
                                })
                                .into()
                            }),
                    ),
            )
    }
}
