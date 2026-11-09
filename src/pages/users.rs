//! 用户 page: user accounts and their pricing groups, behind a 用户 / 用户组
//! segmented switch.
//!
//! - 用户: the list (login, name, group, effective rate, balance, status,
//!   concurrency cap, created), filtered by status and group and searched by
//!   login; a create/edit form; a ledger detail panel per user
//!   ([`LedgerPanel`]: entries, top-up / adjust, reconcile) with a jump to the
//!   Key tab filtered to that user.
//! - 用户组: [`UserGroupsView`]; its groups also feed this page's group
//!   filter and the form's group picker.
//!
//! Data comes from the admin API, not the pushed bootstrap; every write
//! reloads. A rate is typed as a multiplier and parsed with the gateway's
//! `billing::parse_rate` into basis points; the rate field has three states
//! on the wire: untouched, cleared back to the group rate, or set.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Div, ElementId, Entity, EventEmitter, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement, Render, StatefulInteractiveElement as _,
    Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, Sizable as _};

use gateway::http_client::{UserCreate, UserUpdate};
use gateway::store::{GroupRow, UserRow};

use crate::fmt::usd_microus;
use crate::pages::ledger::{BalanceChanged, LedgerPanel};
use crate::pages::user_groups::{
    GroupsChanged, INTERNAL_GROUP, UserGroupsView, rate_error, rate_text,
};
use crate::pages::{
    ActionPage, LoadState, action_error_card, begin_load, card, display_time, editor_row,
    empty_card, error_card, field_label, icon_button, inline_error, loading_card, page_frame,
    refresh_button, segmented,
};
use crate::runtime::GatewayRuntime;

/// Asks the owner to show the Key tab filtered to this user.
pub(crate) struct ShowUserKeys(pub(crate) i64);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    Users,
    Groups,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Enabled,
    Disabled,
}

impl StatusFilter {
    fn admits(self, user: &UserRow) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Enabled => user.enabled,
            StatusFilter::Disabled => !user.enabled,
        }
    }
}

/// How the form sets the user's rate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RateMode {
    /// No override: the group rate applies (on an edit of an overridden user
    /// this clears the override).
    Inherit,
    /// The typed multiplier overrides the group rate.
    Override,
}

/// The page's two group dropdowns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GroupPicker {
    /// The list filter; no selection shows every group.
    Filter,
    /// The form's group.
    Editor,
}

impl GroupPicker {
    fn id(self) -> &'static str {
        match self {
            GroupPicker::Filter => "users-group-filter",
            GroupPicker::Editor => "users-group-picker",
        }
    }

    fn none_label(self) -> &'static str {
        match self {
            GroupPicker::Filter => "全部用户组",
            GroupPicker::Editor => "请选择用户组",
        }
    }
}

/// The edited user's current values; a PATCH carries only what differs.
struct Original {
    display_name: String,
    enabled: bool,
    group_id: i64,
    rate_bp: Option<i64>,
    max_concurrent: i32,
}

/// The create/edit form. One at a time; opening resets every field.
struct UserEditor {
    /// `None` creates a user, `Some(id)` edits that user.
    editing: Option<i64>,
    original: Option<Original>,
    login: Entity<InputState>,
    display_name: Entity<InputState>,
    group: Option<i64>,
    group_list_open: bool,
    rate_mode: RateMode,
    rate: Entity<InputState>,
    max_concurrent: Entity<InputState>,
    enabled: bool,
    saving: bool,
    error: Option<String>,
}

/// What a validated form writes.
struct UserSubmit {
    editing: Option<i64>,
    login: String,
    display_name: Option<String>,
    enabled: Option<bool>,
    group_id: Option<i64>,
    /// Create: `Some(Some(bp))` sets an override. Edit: `None` untouched,
    /// `Some(None)` clears, `Some(Some(bp))` sets.
    rate_bp: Option<Option<i64>>,
    max_concurrent: Option<i32>,
}

/// User management page (the 用户 tab).
pub struct UsersPage {
    segment: Segment,
    active: bool,
    state: LoadState<Vec<UserRow>>,
    groups: Entity<UserGroupsView>,
    action_error: Option<String>,
    /// Login search; every keystroke filters the loaded rows, no new fetch.
    search: Entity<InputState>,
    query: String,
    status_filter: StatusFilter,
    group_filter: Option<i64>,
    group_list_open: bool,
    editor: Option<UserEditor>,
    detail: Option<Entity<LedgerPanel>>,
}

impl EventEmitter<ShowUserKeys> for UsersPage {}

impl UsersPage {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索登录名"));
        cx.subscribe(&search, |page, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                page.query = page.search.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        let groups = cx.new(|_| UserGroupsView::new());
        // User rows carry the group name and rate: a group write reloads them.
        cx.subscribe(&groups, |page, _, _: &GroupsChanged, cx| {
            if page.active {
                page.reload_users(cx);
            }
        })
        .detach();
        cx.observe(&groups, |_, _, cx| cx.notify()).detach();
        Self {
            segment: Segment::Users,
            active: false,
            state: LoadState::Loading,
            groups,
            action_error: None,
            search,
            query: String::new(),
            status_filter: StatusFilter::All,
            group_filter: None,
            group_list_open: false,
            editor: None,
            detail: None,
        }
    }

    pub(crate) fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Tab-bar actions: the segment's create button + refresh.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "users-refresh",
            cx.listener(|this, _, _, cx| this.reload(cx)),
        );
        let create = match self.segment {
            Segment::Users => Button::new("user-create")
                .label("新增用户")
                .on_click(cx.listener(|this, _, window, cx| this.open_create(window, cx))),
            Segment::Groups => {
                Button::new("group-create")
                    .label("新增用户组")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.groups
                            .update(cx, |groups, cx| groups.open_create(window, cx));
                    }))
            }
        };
        vec![
            create
                .icon(Icon::new(IconName::Plus).small())
                .outline()
                .compact()
                .into_any_element(),
            refresh.into_any_element(),
        ]
    }

    fn reload_users(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        begin_load(&mut self.state);
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move { admin.users().await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.state = match result {
                    Ok(users) => LoadState::Loaded(users),
                    Err(error) => LoadState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn user(&self, id: i64) -> Option<&UserRow> {
        match &self.state {
            LoadState::Loaded(users) => users.iter().find(|user| user.id == id),
            _ => None,
        }
    }

    fn pick_segment(&mut self, segment: Segment, cx: &mut Context<Self>) {
        self.segment = segment;
        self.group_list_open = false;
        cx.notify();
    }

    /// The rate input; every keystroke re-renders the page so the inline
    /// parse error follows the text.
    fn rate_input(
        window: &mut Window,
        cx: &mut Context<Self>,
        initial: String,
    ) -> Entity<InputState> {
        let rate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("如 1.25")
                .default_value(initial)
        });
        cx.subscribe(&rate, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();
        rate
    }

    fn open_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let login = cx.new(|cx| InputState::new(window, cx).placeholder("登录名（创建后不可改）"));
        let display_name = cx.new(|cx| InputState::new(window, cx).placeholder("显示名（可选）"));
        let max_concurrent = cx.new(|cx| InputState::new(window, cx).placeholder("默认 4"));
        let focus = login.read(cx).focus_handle(cx);
        self.editor = Some(UserEditor {
            editing: None,
            original: None,
            login,
            display_name,
            group: None,
            group_list_open: false,
            rate_mode: RateMode::Inherit,
            rate: Self::rate_input(window, cx, String::new()),
            max_concurrent,
            enabled: true,
            saving: false,
            error: None,
        });
        self.segment = Segment::Users;
        window.focus(&focus, cx);
        cx.notify();
    }

    fn open_edit(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(user) = self.user(id) else {
            return;
        };
        let original = Original {
            display_name: user.display_name.clone(),
            enabled: user.enabled,
            group_id: user.group_id,
            rate_bp: user.rate_bp,
            max_concurrent: user.max_concurrent,
        };
        let login = cx.new(|cx| InputState::new(window, cx).default_value(user.login.clone()));
        let display_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("显示名（可选）")
                .default_value(original.display_name.clone())
        });
        let rate = Self::rate_input(
            window,
            cx,
            original.rate_bp.map_or_else(String::new, rate_text),
        );
        let max_concurrent = cx.new(|cx| {
            InputState::new(window, cx).default_value(original.max_concurrent.to_string())
        });
        let focus = display_name.read(cx).focus_handle(cx);
        self.editor = Some(UserEditor {
            editing: Some(id),
            group: Some(original.group_id),
            rate_mode: if original.rate_bp.is_some() {
                RateMode::Override
            } else {
                RateMode::Inherit
            },
            enabled: original.enabled,
            original: Some(original),
            login,
            display_name,
            group_list_open: false,
            rate,
            max_concurrent,
            saving: false,
            error: None,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Validate the form into what it writes, or the inline error.
    fn validate(editor: &UserEditor, cx: &Context<Self>) -> Result<UserSubmit, String> {
        let login = editor.login.read(cx).value().trim().to_string();
        if editor.editing.is_none() && login.is_empty() {
            return Err("登录名不能为空".to_string());
        }
        let Some(group_id) = editor.group else {
            return Err("请选择用户组".to_string());
        };
        let rate_bp = match editor.rate_mode {
            RateMode::Inherit => None,
            RateMode::Override => {
                let text = editor.rate.read(cx).value();
                let Some(rate_bp) = gateway::billing::parse_rate(&text) else {
                    return Err(rate_error(&text));
                };
                Some(rate_bp)
            }
        };
        let cap_text = editor.max_concurrent.read(cx).value().trim().to_string();
        let max_concurrent = if cap_text.is_empty() && editor.editing.is_none() {
            None
        } else {
            match cap_text.parse::<i32>() {
                Ok(cap) if cap > 0 => Some(cap),
                _ => return Err("并发上限须为正整数".to_string()),
            }
        };
        let display_name = editor.display_name.read(cx).value().trim().to_string();
        let Some(original) = &editor.original else {
            return Ok(UserSubmit {
                editing: None,
                login,
                display_name: (!display_name.is_empty()).then_some(display_name),
                enabled: None,
                group_id: Some(group_id),
                rate_bp: Some(rate_bp),
                max_concurrent,
            });
        };
        Ok(UserSubmit {
            editing: editor.editing,
            login,
            display_name: (display_name != original.display_name).then_some(display_name),
            enabled: (editor.enabled != original.enabled).then_some(editor.enabled),
            group_id: (group_id != original.group_id).then_some(group_id),
            rate_bp: (rate_bp != original.rate_bp).then_some(rate_bp),
            max_concurrent: max_concurrent.filter(|cap| *cap != original.max_concurrent),
        })
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        if editor.saving {
            return;
        }
        let submit = Self::validate(editor, cx);
        let Some(editor) = &mut self.editor else {
            return;
        };
        let submit = match submit {
            Ok(submit) => submit,
            Err(error) => {
                editor.error = Some(error);
                cx.notify();
                return;
            }
        };
        editor.saving = true;
        editor.error = None;
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let UserSubmit {
                    editing,
                    login,
                    display_name,
                    enabled,
                    group_id,
                    rate_bp,
                    max_concurrent,
                } = submit;
                match (editing, group_id) {
                    (None, Some(group_id)) => {
                        let user = UserCreate {
                            login: &login,
                            display_name: display_name.as_deref(),
                            group_id,
                            rate_bp: rate_bp.flatten(),
                            max_concurrent,
                        };
                        admin.create_user(&user).await.map(|_| ())
                    }
                    (None, None) => Ok(()),
                    (Some(id), group_id) => {
                        let patch = UserUpdate {
                            display_name: display_name.as_deref(),
                            enabled,
                            group_id,
                            rate_bp,
                            max_concurrent,
                        };
                        admin.patch_user(id, &patch).await.map(|_| ())
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

    /// Open (or close, when it is already open) a user's ledger panel.
    fn toggle_detail(&mut self, id: i64, cx: &mut Context<Self>) {
        if self
            .detail
            .as_ref()
            .is_some_and(|panel| panel.read(cx).user_id() == id)
        {
            self.detail = None;
        } else {
            let panel = cx.new(|cx| LedgerPanel::new(id, cx));
            // A top-up or adjustment changes the balance column.
            cx.subscribe(&panel, |page, _, _: &BalanceChanged, cx| {
                page.reload_users(cx)
            })
            .detach();
            self.detail = Some(panel);
        }
        cx.notify();
    }

    fn pick_group(&mut self, picker: GroupPicker, id: Option<i64>, cx: &mut Context<Self>) {
        match picker {
            GroupPicker::Filter => {
                self.group_filter = id;
                self.group_list_open = false;
            }
            GroupPicker::Editor => {
                if let Some(editor) = &mut self.editor {
                    editor.group = id;
                    editor.group_list_open = false;
                }
            }
        }
        cx.notify();
    }

    fn picker_state(&self, picker: GroupPicker) -> (bool, Option<i64>) {
        match picker {
            GroupPicker::Filter => (self.group_list_open, self.group_filter),
            GroupPicker::Editor => self.editor.as_ref().map_or((false, None), |editor| {
                (editor.group_list_open, editor.group)
            }),
        }
    }

    /// A group dropdown: a trigger button plus a floating option list.
    fn group_dropdown(
        &self,
        picker: GroupPicker,
        groups: &[GroupRow],
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let (open, selected) = self.picker_state(picker);
        let label = match selected {
            None => picker.none_label().to_string(),
            Some(id) => groups
                .iter()
                .find(|group| group.id == id)
                .map_or_else(|| format!("#{id}"), group_label),
        };
        let trigger = Button::new(picker.id())
            .label(label)
            .outline()
            .compact()
            .on_click(cx.listener(move |this, _, _, cx| {
                match picker {
                    GroupPicker::Filter => this.group_list_open = !this.group_list_open,
                    GroupPicker::Editor => {
                        if let Some(editor) = &mut this.editor {
                            editor.group_list_open = !editor.group_list_open;
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
        if picker == GroupPicker::Filter {
            options = options.child(Self::group_option(
                picker,
                None,
                selected.is_none(),
                picker.none_label().to_string(),
                cx,
            ));
        }
        for group in groups {
            options = options.child(Self::group_option(
                picker,
                Some(group.id),
                selected == Some(group.id),
                group_label(group),
                cx,
            ));
        }
        // `absolute` takes the list out of document flow; `deferred` draws it
        // after all ancestors so it floats above page content.
        let floating = gpui::deferred(
            gpui::div()
                .absolute()
                .top(px(24.))
                .left(px(0.))
                .w(px(200.))
                .child(options),
        )
        .with_priority(1);
        gpui::div()
            .relative()
            .child(trigger)
            .child(floating)
            .into_any_element()
    }

    fn group_option(
        picker: GroupPicker,
        id: Option<i64>,
        selected: bool,
        text: String,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
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
                text
            })
            .on_click(cx.listener(move |this, _, _, cx| this.pick_group(picker, id, cx)))
    }

    fn editor_card(&self, groups: &[GroupRow], cx: &Context<Self>) -> Option<Div> {
        let editor = self.editor.as_ref()?;
        let creating = editor.editing.is_none();
        let saving = editor.saving;
        let mut form = card(cx).gap_3().child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().foreground)
                .child(if creating {
                    "新增用户"
                } else {
                    "编辑用户"
                }),
        );
        form = form
            .child(editor_row(
                "登录名",
                Input::new(&editor.login).disabled(!creating),
                Some(220.),
                cx,
            ))
            .child(editor_row(
                "显示名",
                Input::new(&editor.display_name),
                Some(220.),
                cx,
            ))
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("用户组", cx))
                    .child(self.group_dropdown(GroupPicker::Editor, groups, cx)),
            )
            .child(Self::rate_row(editor, groups, cx))
            .child(editor_row(
                "并发上限",
                Input::new(&editor.max_concurrent),
                Some(120.),
                cx,
            ));
        if !creating {
            form = form.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("状态", cx))
                    .child(
                        Checkbox::new("user-editor-enabled")
                            .label("启用")
                            .checked(editor.enabled)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                if let Some(editor) = &mut this.editor {
                                    editor.enabled = *checked;
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(error) = &editor.error {
            form = form.child(inline_error(error.clone(), cx));
        }
        Some(
            form.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("user-editor-save")
                            .primary()
                            .compact()
                            .label(if saving { "保存中…" } else { "保存" })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        Button::new("user-editor-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor = None;
                                cx.notify();
                            })),
                    ),
            ),
        )
    }

    /// 继承组倍率 / 自定义倍率, the multiplier input when overriding, and a
    /// live parse error.
    fn rate_row(editor: &UserEditor, groups: &[GroupRow], cx: &Context<Self>) -> Div {
        let group_rate = editor
            .group
            .and_then(|id| groups.iter().find(|group| group.id == id))
            .map(|group| gateway::billing::format_rate(group.rate_bp));
        let clears = editor.rate_mode == RateMode::Inherit
            && editor
                .original
                .as_ref()
                .is_some_and(|original| original.rate_bp.is_some());
        let mut row = gpui::div()
            .flex()
            .items_center()
            .gap_2()
            .child(field_label("倍率", cx))
            .child(rate_mode_switch(editor.rate_mode, cx));
        match editor.rate_mode {
            RateMode::Inherit => {
                let hint = match (group_rate, clears) {
                    (Some(rate), true) => format!("组倍率 {rate}（保存后清除覆盖）"),
                    (Some(rate), false) => format!("组倍率 {rate}"),
                    (None, true) => "保存后清除覆盖".to_string(),
                    (None, false) => String::new(),
                };
                row = row.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                );
            }
            RateMode::Override => {
                let text = editor.rate.read(cx).value();
                row = row.child(Input::new(&editor.rate).w(px(120.))).child("×");
                if !text.trim().is_empty() && gateway::billing::parse_rate(&text).is_none() {
                    row = row.child(inline_error(rate_error(&text), cx));
                }
            }
        }
        row
    }

    fn user_table(&self, users: &[&UserRow], cx: &Context<Self>) -> Div {
        let open_detail = self.detail.as_ref().map(|panel| panel.read(cx).user_id());
        let mut table = gpui::div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(
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
                    .child(gpui::div().w(px(110.)).child("登录名"))
                    .child(gpui::div().flex_1().child("显示名"))
                    .child(gpui::div().w(px(110.)).child("用户组"))
                    .child(gpui::div().w(px(96.)).child("倍率"))
                    .child(gpui::div().w(px(96.)).child("余额"))
                    .child(gpui::div().w(px(44.)).child("状态"))
                    .child(gpui::div().w(px(44.)).child("并发"))
                    .child(gpui::div().w(px(108.)).child("创建时间"))
                    .child(gpui::div().w(px(64.)).child("操作")),
            );
        for user in users {
            table = table.child(Self::user_row(user, open_detail == Some(user.id), cx));
        }
        table
    }

    fn user_row(user: &UserRow, detail_open: bool, cx: &Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let id = user.id;
        let rate = gateway::billing::format_rate(gateway::billing::effective_rate_bp(
            user.rate_bp,
            user.group_rate_bp,
        ));
        let group = group_name_label(&user.group_name);
        let (status_text, status_color) = if user.enabled {
            ("启用", gpui::green())
        } else {
            ("停用", gpui::red())
        };
        gpui::div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .text_size(px(12.))
            .when(detail_open, |row| row.bg(cx.theme().accent))
            .child(
                gpui::div()
                    .w(px(110.))
                    .text_color(cx.theme().foreground)
                    .child(user.login.clone()),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_color(muted)
                    .child(user.display_name.clone()),
            )
            .child(gpui::div().w(px(110.)).text_color(muted).child(group))
            .child(
                gpui::div()
                    .w(px(96.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(muted)
                    .child(rate)
                    .when(user.rate_bp.is_some(), |cell| {
                        cell.child(
                            gpui::div()
                                .px_1()
                                .rounded_sm()
                                .text_size(px(10.))
                                .bg(cx.theme().muted)
                                .text_color(cx.theme().foreground)
                                .child("覆盖"),
                        )
                    }),
            )
            .child(
                gpui::div()
                    .w(px(96.))
                    .text_color(if user.balance_microus <= 0 {
                        cx.theme().danger
                    } else {
                        cx.theme().foreground
                    })
                    .child(usd_microus(user.balance_microus)),
            )
            .child(
                gpui::div()
                    .w(px(44.))
                    .text_color(status_color)
                    .child(status_text),
            )
            .child(
                gpui::div()
                    .w(px(44.))
                    .text_color(muted)
                    .child(user.max_concurrent.to_string()),
            )
            .child(
                gpui::div()
                    .w(px(108.))
                    .text_color(muted)
                    .child(display_time(user.created_at)),
            )
            .child(
                gpui::div()
                    .w(px(64.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon_button(
                        ElementId::NamedInteger("edit-user-btn".into(), id as u64),
                        IconName::Pencil,
                        "编辑",
                        cx.listener(move |this, _, window, cx| this.open_edit(id, window, cx)),
                    ))
                    .child(icon_button(
                        ElementId::NamedInteger("user-detail-btn".into(), id as u64),
                        IconName::FileText,
                        if detail_open {
                            "收起流水"
                        } else {
                            "流水与对账"
                        },
                        cx.listener(move |this, _, _, cx| this.toggle_detail(id, cx)),
                    )),
            )
    }

    /// The open ledger panel under a header naming the user, with the jump
    /// to their keys and a close button.
    fn detail_card(&self, cx: &Context<Self>) -> Option<Div> {
        let panel = self.detail.as_ref()?;
        let id = panel.read(cx).user_id();
        let name = self.user(id).map_or_else(
            || format!("#{id}"),
            |user| {
                if user.display_name.is_empty() {
                    user.login.clone()
                } else {
                    format!("{}（{}）", user.login, user.display_name)
                }
            },
        );
        Some(
            card(cx)
                .gap_3()
                .child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            gpui::div()
                                .flex_1()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(cx.theme().foreground)
                                .child(format!("{name} 的流水")),
                        )
                        .child(
                            Button::new("user-show-keys")
                                .icon(Icon::new(IconName::ExternalLink).small())
                                .label("查看该用户的 key")
                                .ghost()
                                .compact()
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(ShowUserKeys(id));
                                })),
                        )
                        .child(icon_button(
                            "user-detail-close",
                            IconName::Close,
                            "关闭",
                            cx.listener(|this, _, _, cx| {
                                this.detail = None;
                                cx.notify();
                            }),
                        )),
                )
                .child(panel.clone()),
        )
    }

    fn users_body(&self, groups: &[GroupRow], cx: &Context<Self>) -> Div {
        let mut content = gpui::div().flex().flex_col().gap_4().child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .w(px(220.))
                        .child(Input::new(&self.search).small()),
                )
                .child(self.group_dropdown(GroupPicker::Filter, groups, cx)),
        );
        if let Some(editor) = self.editor_card(groups, cx) {
            content = content.child(editor);
        }
        if let Some(detail) = self.detail_card(cx) {
            content = content.child(detail);
        }
        match &self.state {
            LoadState::Loading => content.child(loading_card(cx)),
            LoadState::Failed(error) => content.child(error_card(error, cx)),
            LoadState::Loaded(users) => {
                let query = self.query.trim();
                let visible: Vec<&UserRow> = users
                    .iter()
                    .filter(|user| self.status_filter.admits(user))
                    .filter(|user| self.group_filter.is_none_or(|id| user.group_id == id))
                    .filter(|user| query.is_empty() || user.login.contains(query))
                    .collect();
                if users.is_empty() {
                    content.child(empty_card("暂无用户", "点右上角「新增用户」创建。", cx))
                } else if visible.is_empty() {
                    content.child(empty_card(
                        "没有匹配的用户",
                        "换个状态、用户组或搜索词试试。",
                        cx,
                    ))
                } else {
                    content.child(self.user_table(&visible, cx))
                }
            }
        }
    }
}

impl ActionPage for UsersPage {
    fn action_error_slot(&mut self) -> &mut Option<String> {
        &mut self.action_error
    }

    /// Reload the users, the groups (filter, picker and the 用户组 segment)
    /// and the open ledger panel.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        self.reload_users(cx);
        self.groups.update(cx, |groups, cx| groups.reload(cx));
        if let Some(panel) = &self.detail {
            panel.update(cx, |panel, cx| panel.reload(cx));
        }
    }
}

impl Render for UsersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut header = gpui::div().flex().items_center().gap_3().child(segmented(
            "users-segment",
            &[(Segment::Users, "用户"), (Segment::Groups, "用户组")],
            self.segment,
            cx,
            Self::pick_segment,
        ));
        if self.segment == Segment::Users {
            header = header.child(segmented(
                "users-status",
                &[
                    (StatusFilter::All, "全部"),
                    (StatusFilter::Enabled, "启用"),
                    (StatusFilter::Disabled, "停用"),
                ],
                self.status_filter,
                cx,
                |page, filter, cx| {
                    page.status_filter = filter;
                    cx.notify();
                },
            ));
        }
        let mut content = gpui::div().flex().flex_col().gap_4().child(header);
        if let Some(error) = &self.action_error {
            content = content.child(action_error_card(error, cx));
        }
        content = match self.segment {
            Segment::Users => {
                let groups = self.groups.read(cx).groups().unwrap_or(&[]);
                content.child(self.users_body(groups, cx))
            }
            Segment::Groups => content.child(self.groups.clone()),
        };
        page_frame("", "", cx).child(content)
    }
}

/// A group's picker text; the built-in free group says so.
fn group_label(group: &GroupRow) -> String {
    group_name_label(&group.name)
}

fn group_name_label(name: &str) -> String {
    if name == INTERNAL_GROUP {
        format!("{name}（内部免费）")
    } else {
        name.to_string()
    }
}

/// The form's 继承组倍率 / 自定义倍率 switch.
fn rate_mode_switch(mode: RateMode, cx: &Context<UsersPage>) -> Div {
    let mut track = gpui::div().flex().gap_1();
    for (value, label) in [
        (RateMode::Inherit, "继承组倍率"),
        (RateMode::Override, "自定义倍率"),
    ] {
        let id = match value {
            RateMode::Inherit => "user-rate-inherit",
            RateMode::Override => "user-rate-override",
        };
        let button =
            Button::new(id)
                .label(label)
                .compact()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor {
                        editor.rate_mode = value;
                    }
                    cx.notify();
                }));
        track = track.child(if value == mode {
            button.primary()
        } else {
            button.outline()
        });
    }
    track
}
