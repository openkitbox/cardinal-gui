//! Key page: gateway API key management. Each row shows the owner, the full
//! plaintext key (copyable), status, expiry and the quota spent against its
//! cap. Keys are stored in plaintext by the gateway so an administrator can
//! always read them back; this page never masks them.
//!
//! Data comes from the admin API (`users` for the owner picker and the user
//! filter, `gateway_keys` for the list), not from the pushed bootstrap
//! document; every write reloads. An expiry is picked as a calendar day and
//! submitted as 23:59:59 of that day in the GUI display timezone; a quota is
//! sent as the typed decimal USD string and parsed by the gateway — no float
//! arithmetic happens here.

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Div, ElementId, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement, Render, StatefulInteractiveElement as _, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::calendar::{Date as PickerDate, Matcher};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::date_picker::{DatePicker, DatePickerState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, Sizable as _};

use gateway::http_client::GatewayKeyUpdate;
use gateway::store::{GatewayKeyRow, UserRow};

use crate::pages::{
    ActionPage, LoadState, action_error_card, begin_load, card, empty_card, error_card,
    loading_card, page_frame, refresh_button, run_write,
};
use crate::runtime::GatewayRuntime;

/// The list and the users the pickers need, fetched together so a row always
/// resolves its owner against a snapshot taken at the same time.
struct KeySnapshot {
    keys: Vec<GatewayKeyRow>,
    users: Vec<UserRow>,
}

/// The create/edit form. One editor at a time; opening it resets every field
/// instead of keeping stale input from the previous key.
struct KeyEditor {
    /// `None` creates a key, `Some(id)` edits that key.
    editing: Option<i64>,
    /// The edited key's current terms, kept so a PATCH only carries fields
    /// the user actually changed (an untouched past expiry must not be
    /// revalidated, and an untouched quota must not be rewritten).
    original_expiry: Option<DateTime<Utc>>,
    original_quota: Option<i64>,
    original_enabled: bool,
    /// Owner of a new key; preselected to the `owner` user. `None` (no users
    /// loaded) leaves the choice to the gateway, which also defaults to
    /// `owner`.
    owner: Option<i64>,
    label: Entity<InputState>,
    expiry: Entity<DatePickerState>,
    quota: Entity<InputState>,
    enabled: bool,
    saving: bool,
    error: Option<String>,
    owner_list_open: bool,
}

/// The page's two user dropdowns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UserPicker {
    /// The list filter; no selection shows every user's keys.
    Filter,
    /// The create form's owner.
    Owner,
}

impl UserPicker {
    fn id(self) -> &'static str {
        match self {
            UserPicker::Filter => "keys-user-filter",
            UserPicker::Owner => "keys-owner-picker",
        }
    }

    /// Trigger text with no user selected.
    fn none_label(self) -> &'static str {
        match self {
            UserPicker::Filter => "全部用户",
            UserPicker::Owner => "owner（默认）",
        }
    }
}

/// Gateway key management page (the Key tab).
pub struct GatewayKeysPage {
    state: LoadState<KeySnapshot>,
    action_error: Option<String>,
    /// Label search; every keystroke filters the loaded rows, no new fetch.
    search: Entity<InputState>,
    query: String,
    user_filter: Option<i64>,
    user_list_open: bool,
    editor: Option<KeyEditor>,
}

impl GatewayKeysPage {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索 label"));
        cx.subscribe(&search, |page, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                page.query = page.search.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        Self {
            state: LoadState::Loading,
            action_error: None,
            search,
            query: String::new(),
            user_filter: None,
            user_list_open: false,
            editor: None,
        }
    }

    /// Tab-bar actions (right-aligned): add-key button + refresh button.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "keys-refresh",
            cx.listener(|this, _, _, cx| this.reload(cx)),
        );
        match &self.state {
            LoadState::Loaded(_) => vec![
                Button::new("key-create")
                    .icon(Icon::new(IconName::Plus).small())
                    .label("新增 Key")
                    .outline()
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_create(window, cx);
                    }))
                    .into_any_element(),
                refresh.into_any_element(),
            ],
            _ => vec![refresh.into_any_element()],
        }
    }

    /// One date picker with past days disabled: the gateway refuses an expiry
    /// at or before now, and the picker submits the day's 23:59:59.
    fn date_picker(window: &mut Window, cx: &mut App) -> Entity<DatePickerState> {
        let today = Utc::now()
            .with_timezone(&crate::ui_tz::display_tz())
            .date_naive();
        cx.new(|cx| {
            DatePickerState::new(window, cx)
                .first_day_of_week(chrono::Weekday::Mon)
                .disabled_matcher(Matcher::custom(move |day| *day < today))
        })
    }

    fn open_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = self.users().and_then(|users| {
            users
                .iter()
                .find(|user| user.login == "owner")
                .or_else(|| users.first())
                .map(|user| user.id)
        });
        let label =
            cx.new(|cx| InputState::new(window, cx).placeholder("label（标识 key 的归属）"));
        let quota =
            cx.new(|cx| InputState::new(window, cx).placeholder("不限（USD，如 10 或 0.5）"));
        let focus = label.read(cx).focus_handle(cx);
        self.editor = Some(KeyEditor {
            editing: None,
            original_expiry: None,
            original_quota: None,
            original_enabled: true,
            owner,
            label,
            expiry: Self::date_picker(window, cx),
            quota,
            enabled: true,
            saving: false,
            error: None,
            owner_list_open: false,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// The loaded row with this id; row buttons carry only the id so a render
    /// never copies key data into its closures.
    fn key(&self, id: i64) -> Option<&GatewayKeyRow> {
        match &self.state {
            LoadState::Loaded(snapshot) => snapshot.keys.iter().find(|key| key.id == id),
            _ => None,
        }
    }

    /// Put the full plaintext key on the clipboard.
    fn copy_key(&self, id: i64, cx: &mut Context<Self>) {
        if let Some(key) = self.key(id) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(key.key.clone()));
        }
    }

    fn open_edit(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.key(id) else {
            return;
        };
        let label = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("label（标识 key 的归属）")
                .default_value(key.label.clone())
        });
        let quota = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("不限（USD，如 10 或 0.5）")
                .default_value(
                    key.quota_microus
                        .map_or_else(String::new, gateway::billing::format_usd),
                )
        });
        let expiry = Self::date_picker(window, cx);
        let picked = key
            .expires_at
            .map(|at| at.with_timezone(&crate::ui_tz::display_tz()).date_naive());
        expiry.update(cx, |state, cx| {
            state.set_date(PickerDate::Single(picked), window, cx);
        });
        let focus = label.read(cx).focus_handle(cx);
        self.editor = Some(KeyEditor {
            editing: Some(key.id),
            original_expiry: key.expires_at,
            original_quota: key.quota_microus,
            original_enabled: key.enabled,
            owner: Some(key.user_id),
            label,
            expiry,
            quota,
            enabled: key.enabled,
            saving: false,
            error: None,
            owner_list_open: false,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    fn users(&self) -> Option<&[UserRow]> {
        match &self.state {
            LoadState::Loaded(snapshot) => Some(&snapshot.users),
            _ => None,
        }
    }

    /// The expiry the editor submits: the picked day's 23:59:59 in the display
    /// timezone, converted to UTC. `None` means never expires.
    fn picked_expiry(editor: &KeyEditor, cx: &App) -> Option<DateTime<Utc>> {
        let PickerDate::Single(Some(day)) = editor.expiry.read(cx).date() else {
            return None;
        };
        day.and_hms_opt(23, 59, 59)
            .and_then(|local| {
                crate::ui_tz::display_tz()
                    .from_local_datetime(&local)
                    .single()
            })
            .map(|at| at.with_timezone(&Utc))
    }

    /// The typed quota: empty means unlimited, anything else is sent verbatim
    /// (the gateway parses the decimal USD string).
    fn typed_quota(editor: &KeyEditor, cx: &App) -> Option<String> {
        let text = editor.quota.read(cx).value().trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.saving {
            return;
        }
        let label = editor.label.read(cx).value().trim().to_string();
        if let Err(error) = gateway::key_terms::label(&label) {
            editor.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let quota = Self::typed_quota(editor, cx);
        if let Err(error) = gateway::key_terms::quota(quota.as_deref()) {
            editor.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let expires_at = Self::picked_expiry(editor, cx);
        let editing = editor.editing;
        let enabled = editor.enabled;
        let owner = editor.owner;
        // Edits send only the changed terms: the gateway neither revalidates
        // nor rewrites an untouched one. The expiry is edited as a day, so an
        // untouched day keeps its original timestamp (and an already expired
        // key can still be renamed or disabled).
        let original_day = editor
            .original_expiry
            .map(|at| at.with_timezone(&crate::ui_tz::display_tz()).date_naive());
        let PickerDate::Single(picked_day) = editor.expiry.read(cx).date() else {
            return;
        };
        let expiry_changed = editing.is_none() || picked_day != original_day;
        if expiry_changed && expires_at.is_some_and(|at| at <= Utc::now()) {
            editor.error = Some("过期时间必须晚于现在".to_string());
            cx.notify();
            return;
        }
        let parsed_quota = quota.as_deref().and_then(gateway::billing::parse_usd);
        let quota_changed = parsed_quota != editor.original_quota;
        let enabled_changed = enabled != editor.original_enabled;
        editor.saving = true;
        editor.error = None;
        cx.notify();

        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                match editing {
                    None => admin
                        .create_gateway_key(owner, &label, expires_at, quota.as_deref())
                        .await
                        .map(|_| ()),
                    Some(id) => {
                        let update = GatewayKeyUpdate {
                            label: Some(&label),
                            enabled: enabled_changed.then_some(enabled),
                            expires_at: expiry_changed.then_some(expires_at),
                            quota_usd: quota_changed.then_some(quota.as_deref()),
                        };
                        admin.patch_gateway_key(id, &update).await
                    }
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| match result {
                Ok(()) => {
                    page.editor = None;
                    page.action_error = None;
                    page.reload(cx);
                }
                Err(error) => {
                    if let Some(editor) = &mut page.editor {
                        editor.saving = false;
                        editor.error = Some(error);
                    }
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

    /// Show only `user_id`'s keys (`None` = every user's), as if picked in
    /// the filter dropdown; used by the 用户 page's jump to this tab.
    pub(crate) fn set_user_filter(&mut self, user_id: Option<i64>, cx: &mut Context<Self>) {
        self.pick_user(UserPicker::Filter, user_id, cx);
    }

    /// Pick a user in one of the two dropdowns.
    fn pick_user(&mut self, picker: UserPicker, id: Option<i64>, cx: &mut Context<Self>) {
        match picker {
            UserPicker::Filter => {
                self.user_filter = id;
                self.user_list_open = false;
            }
            UserPicker::Owner => {
                if let Some(editor) = &mut self.editor {
                    editor.owner = id;
                    editor.owner_list_open = false;
                }
            }
        }
        cx.notify();
    }

    /// Whether `picker`'s option list is open, and the user it has selected.
    fn picker_state(&self, picker: UserPicker) -> (bool, Option<i64>) {
        match picker {
            UserPicker::Filter => (self.user_list_open, self.user_filter),
            UserPicker::Owner => self.editor.as_ref().map_or((false, None), |editor| {
                (editor.owner_list_open, editor.owner)
            }),
        }
    }

    /// A user dropdown: a trigger button plus a floating option list.
    fn user_dropdown(&mut self, picker: UserPicker, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (open, selected) = self.picker_state(picker);
        let label = match selected {
            None => picker.none_label().to_string(),
            Some(id) => self
                .users()
                .and_then(|users| users.iter().find(|user| user.id == id))
                .map_or_else(|| format!("#{id}"), |user| user.login.clone()),
        };
        let trigger = Button::new(picker.id())
            .label(label)
            .outline()
            .compact()
            .on_click(cx.listener(move |this, _, _, cx| {
                match picker {
                    UserPicker::Filter => this.user_list_open = !this.user_list_open,
                    UserPicker::Owner => {
                        if let Some(editor) = &mut this.editor {
                            editor.owner_list_open = !editor.owner_list_open;
                        }
                    }
                }
                cx.notify();
            }));
        if !open {
            return trigger.into_any_element();
        }
        let mut options = gpui::div()
            .id(ElementId::NamedInteger(picker.id().into(), 0))
            .flex()
            .flex_col()
            .border_1()
            .rounded_md()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .max_h(px(240.))
            .overflow_y_scroll();
        if picker == UserPicker::Filter {
            options = options.child(self.user_option(picker, None, picker.none_label(), cx));
        }
        if let Some(users) = self.users() {
            for user in users {
                options = options.child(self.user_option(picker, Some(user.id), &user.login, cx));
            }
        }
        // `absolute` takes the list out of document flow; `deferred` draws it
        // after all ancestors so it floats above page content.
        let floating = gpui::deferred(
            gpui::div()
                .absolute()
                .top(px(24.))
                .left(px(0.))
                .w(px(180.))
                .child(options),
        )
        .with_priority(1);
        gpui::div()
            .relative()
            .child(trigger)
            .child(floating)
            .into_any_element()
    }

    /// One clickable line in a user dropdown.
    fn user_option(
        &self,
        picker: UserPicker,
        id: Option<i64>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let selected = self.picker_state(picker).1 == id;
        let option_id = ElementId::from(gpui::SharedString::from(format!(
            "{}-{}",
            picker.id(),
            id.map_or_else(|| "all".to_string(), |id| id.to_string())
        )));
        gpui::div()
            .id(option_id)
            .cursor_pointer()
            .px_2()
            .py_1()
            .text_size(px(12.))
            .text_color(if selected {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            })
            .when(selected, |this| {
                this.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .hover(|this| this.bg(cx.theme().accent))
            .child(if selected {
                format!("✓ {text}")
            } else {
                text.to_string()
            })
            .on_click(cx.listener(move |this, _, _, cx| this.pick_user(picker, id, cx)))
    }

    /// The create/edit card: owner (create only), label, expiry day, quota and
    /// the enabled switch.
    fn editor_card(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let editor = self.editor.as_ref()?;
        let creating = editor.editing.is_none();
        let title = if creating { "新增 Key" } else { "编辑 Key" };
        let saving = editor.saving;
        let label = editor.label.clone();
        let quota = editor.quota.clone();
        let expiry = editor.expiry.clone();
        let enabled = editor.enabled;
        let error = editor.error.clone();

        let mut form = card(cx).gap_3().child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().foreground)
                .child(title),
        );
        if creating {
            form = form.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("归属用户", cx))
                    .child(self.user_dropdown(UserPicker::Owner, cx)),
            );
        }
        form = form
            .child(editor_row("Label", Input::new(&label), None, cx))
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("过期时间", cx))
                    .child(
                        DatePicker::new(&expiry)
                            .placeholder("永不过期")
                            .cleanable(true)
                            .small()
                            .w(px(200.)),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("所选日期 23:59:59（界面时区）"),
                    ),
            )
            .child(editor_row("额度 USD", Input::new(&quota), Some(200.), cx));
        if !creating {
            form = form.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("状态", cx))
                    .child(
                        Checkbox::new("key-editor-enabled")
                            .label("启用")
                            .checked(enabled)
                            .on_click(cx.listener(|this, checked, _, cx| {
                                if let Some(editor) = &mut this.editor {
                                    editor.enabled = *checked;
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(error) = error {
            form = form.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(error),
            );
        }
        Some(
            form.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("key-editor-save")
                            .primary()
                            .compact()
                            .label(if saving { "保存中…" } else { "保存" })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        Button::new("key-editor-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
                    ),
            ),
        )
    }

    /// The key table: label, owner, full key, status, expiry, quota, created,
    /// last used and the row actions.
    fn key_table(&self, keys: &[&GatewayKeyRow], cx: &mut Context<Self>) -> Div {
        let mut table = gpui::div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border);
        table = table.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(gpui::div().w(px(120.)).child("Label"))
                .child(gpui::div().w(px(80.)).child("归属"))
                .child(gpui::div().flex_1().child("Key"))
                .child(gpui::div().w(px(44.)).child("状态"))
                .child(gpui::div().w(px(88.)).child("过期时间"))
                .child(gpui::div().w(px(150.)).child("已用 / 额度"))
                .child(gpui::div().w(px(108.)).child("创建时间"))
                .child(gpui::div().w(px(108.)).child("最后使用"))
                .child(gpui::div().w(px(96.)).child("操作")),
        );
        for key in keys {
            table = table.child(self.key_row(key, cx));
        }
        table
    }

    fn key_row(&self, key: &GatewayKeyRow, cx: &mut Context<Self>) -> Div {
        let now = Utc::now();
        let expired = key.expires_at.is_some_and(|at| at <= now);
        let used_up = key
            .quota_microus
            .is_some_and(|cap| key.spent_microus >= cap);
        let (status_text, status_color) = if key.enabled {
            ("启用", gpui::green())
        } else {
            ("暂停", gpui::red())
        };
        let toggle_label = if key.enabled { "暂停" } else { "启用" };
        let key_id = key.id;
        let enabled = !key.enabled;
        let index = key.id as u64;

        gpui::div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .text_size(px(12.))
            .child(
                gpui::div()
                    .w(px(120.))
                    .text_color(cx.theme().foreground)
                    .child(key.label.clone()),
            )
            .child(
                gpui::div()
                    .w(px(80.))
                    .text_color(cx.theme().muted_foreground)
                    .child(key.login.clone()),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .font_family("monospace")
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(key.key.clone()),
            )
            .child(
                gpui::div()
                    .w(px(44.))
                    .text_color(status_color)
                    .child(status_text),
            )
            .child(
                gpui::div()
                    .w(px(88.))
                    .text_color(if expired {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(key.expires_at.map_or_else(
                        || "永不过期".to_string(),
                        |at| {
                            at.with_timezone(&crate::ui_tz::display_tz())
                                .format("%Y-%m-%d")
                                .to_string()
                        },
                    )),
            )
            .child(
                gpui::div()
                    .w(px(150.))
                    .text_color(if used_up {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(quota_text(key)),
            )
            .child(
                gpui::div()
                    .w(px(108.))
                    .text_color(cx.theme().muted_foreground)
                    .child(display_time(key.created_at)),
            )
            .child(
                gpui::div()
                    .w(px(108.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        key.last_used_at
                            .map_or_else(|| "—".to_string(), display_time),
                    ),
            )
            .child(
                gpui::div()
                    .w(px(96.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon_button(
                        ElementId::NamedInteger("copy-key-btn".into(), index),
                        IconName::Copy,
                        "复制完整 Key",
                        cx.listener(move |this, _, _, cx| this.copy_key(key_id, cx)),
                    ))
                    .child(icon_button(
                        ElementId::NamedInteger("edit-key-btn".into(), index),
                        IconName::Pencil,
                        "编辑",
                        cx.listener(move |this, _, window, cx| this.open_edit(key_id, window, cx)),
                    ))
                    .child(icon_button(
                        ElementId::NamedInteger("toggle-key-btn".into(), index),
                        if key.enabled {
                            IconName::Pause
                        } else {
                            IconName::Play
                        },
                        toggle_label,
                        cx.listener(move |this, _, _, cx| this.toggle_key(key_id, enabled, cx)),
                    )),
            )
    }
}

impl ActionPage for GatewayKeysPage {
    fn action_error_slot(&mut self) -> &mut Option<String> {
        &mut self.action_error
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        begin_load(&mut self.state);
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move {
                // Both reads happen on the same connection; the owner names and
                // the picker options describe the same snapshot as the rows.
                let keys = admin.gateway_keys(None).await?;
                let users = admin.users().await?;
                Ok(KeySnapshot { keys, users })
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.state = match result {
                    Ok(snapshot) => LoadState::Loaded(snapshot),
                    Err(error) => LoadState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for GatewayKeysPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = gpui::div().flex().flex_col().gap_4();

        if let Some(error) = &self.action_error {
            content = content.child(action_error_card(error, cx));
        }

        let filter = self.user_dropdown(UserPicker::Filter, cx);
        content = content.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .w(px(260.))
                        .child(Input::new(&self.search).small()),
                )
                .child(filter),
        );

        if let Some(editor) = self.editor_card(cx) {
            content = content.child(editor);
        }

        match &self.state {
            LoadState::Loading => {
                content = content.child(loading_card(cx));
            }
            LoadState::Failed(error) => {
                content = content.child(error_card(error, cx));
            }
            LoadState::Loaded(snapshot) => {
                let query = self.query.trim();
                let visible: Vec<&GatewayKeyRow> = snapshot
                    .keys
                    .iter()
                    .filter(|key| self.user_filter.is_none_or(|id| key.user_id == id))
                    .filter(|key| query.is_empty() || key.label.contains(query))
                    .collect();
                if snapshot.keys.is_empty() {
                    content =
                        content.child(empty_card("暂无 Key", "点右上角「新增 Key」创建。", cx));
                } else if visible.is_empty() {
                    content =
                        content.child(empty_card("没有匹配的 Key", "换个用户或搜索词试试。", cx));
                } else {
                    content = content.child(self.key_table(&visible, cx));
                }
            }
        }

        page_frame("", "", cx).child(content)
    }
}

/// `$spent / $cap`; a key without a cap shows its spend against 不限.
fn quota_text(key: &GatewayKeyRow) -> String {
    let spent = crate::fmt::usd_exact(key.spent_microus);
    match key.quota_microus {
        Some(cap) => format!("{spent} / {}", crate::fmt::usd_exact(cap)),
        None => format!("{spent} / 不限"),
    }
}

fn display_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&crate::ui_tz::display_tz())
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn field_label(label: &'static str, cx: &App) -> Div {
    gpui::div()
        .w(px(72.))
        .flex_shrink_0()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// One labeled row of the editor card; `width` sizes the input to its content.
fn editor_row(label: &'static str, input: Input, width: Option<f32>, cx: &App) -> Div {
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

/// A ghost icon button with a tooltip (copy / edit / pause-play).
fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    tooltip: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id)
        .icon(Icon::new(icon).small())
        .ghost()
        .compact()
        .tooltip(tooltip)
        .on_click(on_click)
}
