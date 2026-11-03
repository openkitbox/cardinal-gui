//! 用户组 segment of the 用户 page: pricing groups with their rate, the
//! routes their users may call (`route_aliases` null = every route) and the
//! member count; create and edit.
//!
//! The route picker lists the aliases from `client.routes()`, read together
//! with the groups. A whitelisted alias that no longer exists as a route stays
//! listed (marked 已删除) so saving an unrelated change does not silently
//! drop it. The built-in `internal` group (rate 0, the owner and every
//! internal caller) is labelled 内部免费; changing its rate or routes asks
//! for confirmation first.

use gpui::{
    AppContext as _, Context, Div, ElementId, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement, Render, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, WindowExt as _};

use gateway::http_client::{UserGroupCreate, UserGroupUpdate};
use gateway::store::GroupRow;

use crate::pages::{
    LoadState, card, editor_row, empty_card, error_card, field_label, icon_button, inline_error,
    loading_card,
};
use crate::runtime::GatewayRuntime;

/// Name of the built-in free group the gateway seeds (migration 0012).
pub(crate) const INTERNAL_GROUP: &str = "internal";

/// Groups and the route aliases the picker offers, read together.
pub(crate) struct GroupSnapshot {
    pub(crate) groups: Vec<GroupRow>,
    aliases: Vec<String>,
}

/// The create/edit form.
struct GroupEditor {
    /// `None` creates a group, `Some(id)` edits it.
    editing: Option<i64>,
    /// The edited group's name, rate and routes, kept so a PATCH carries only
    /// what changed and the `internal` confirm knows whether it applies.
    original_name: String,
    original_rate_bp: i64,
    original_routes: Option<Vec<String>>,
    name: Entity<InputState>,
    rate: Entity<InputState>,
    /// `None` = every route; `Some` = the checked aliases.
    routes: Option<Vec<String>>,
    saving: bool,
    error: Option<String>,
}

/// What a validated form submits.
#[derive(Clone)]
struct GroupSubmit {
    editing: Option<i64>,
    name: String,
    rate_bp: i64,
    routes: Option<Vec<String>>,
    name_changed: bool,
    rate_changed: bool,
    routes_changed: bool,
}

/// Emitted after a group write: user rows carry the group name and rate,
/// so the user list reloads.
pub(crate) struct GroupsChanged;

/// The 用户组 segment, owned by the users page (which also reads its groups
/// for the user filter and picker).
pub(crate) struct UserGroupsView {
    state: LoadState<GroupSnapshot>,
    editor: Option<GroupEditor>,
}

impl EventEmitter<GroupsChanged> for UserGroupsView {}

impl UserGroupsView {
    pub(crate) fn new() -> Self {
        Self {
            state: LoadState::Loading,
            editor: None,
        }
    }

    /// The loaded groups (also the user page's group filter and picker).
    pub(crate) fn groups(&self) -> Option<&[GroupRow]> {
        match &self.state {
            LoadState::Loaded(snapshot) => Some(&snapshot.groups),
            _ => None,
        }
    }

    fn group(&self, id: i64) -> Option<&GroupRow> {
        self.groups()?.iter().find(|group| group.id == id)
    }

    pub(crate) fn open_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("组名"));
        let rate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("倍率，如 1 或 1.25")
                .default_value("1")
        });
        let focus = name.read(cx).focus_handle(cx);
        self.editor = Some(GroupEditor {
            editing: None,
            original_name: String::new(),
            original_rate_bp: 0,
            original_routes: None,
            name,
            rate,
            routes: None,
            saving: false,
            error: None,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn open_edit(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.group(id) else {
            return;
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("组名")
                .default_value(group.name.clone())
        });
        let rate = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("倍率，如 1 或 1.25")
                .default_value(rate_text(group.rate_bp))
        });
        let editor = GroupEditor {
            editing: Some(group.id),
            original_name: group.name.clone(),
            original_rate_bp: group.rate_bp,
            original_routes: group.route_aliases.clone(),
            routes: group.route_aliases.clone(),
            name,
            rate,
            saving: false,
            error: None,
        };
        let focus = editor.name.read(cx).focus_handle(cx);
        self.editor = Some(editor);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Validate the form into what it would submit, or set its inline error.
    fn validate(&mut self, cx: &mut Context<Self>) -> Option<GroupSubmit> {
        let editor = self.editor.as_mut()?;
        if editor.saving {
            return None;
        }
        let name = editor.name.read(cx).value().trim().to_string();
        let rate_text = editor.rate.read(cx).value().to_string();
        let error = if name.is_empty() {
            Some("组名不能为空".to_string())
        } else if editor.routes.as_ref().is_some_and(Vec::is_empty) {
            Some("至少勾选一条路由，或选「全部路由」".to_string())
        } else {
            None
        };
        if let Some(error) = error {
            editor.error = Some(error);
            cx.notify();
            return None;
        }
        let Some(rate_bp) = gateway::billing::parse_rate(&rate_text) else {
            editor.error = Some(rate_error(&rate_text));
            cx.notify();
            return None;
        };
        editor.error = None;
        cx.notify();
        let creating = editor.editing.is_none();
        Some(GroupSubmit {
            editing: editor.editing,
            name_changed: creating || name != editor.original_name,
            rate_changed: creating || rate_bp != editor.original_rate_bp,
            routes_changed: creating || editor.routes != editor.original_routes,
            name,
            rate_bp,
            routes: editor.routes.clone(),
        })
    }

    /// Validate, confirm when the internal group's pricing or routes change,
    /// then write.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(submit) = self.validate(cx) else {
            return;
        };
        let internal = self.editor.as_ref().is_some_and(|editor| {
            editor.editing.is_some() && editor.original_name == INTERNAL_GROUP
        });
        if !(internal && (submit.rate_changed || submit.routes_changed)) {
            self.submit(submit, cx);
            return;
        }
        let view = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let submit = submit.clone();
            dialog
                .confirm()
                .title("修改内部免费组?")
                .description(
                    "internal 组承载 owner 及所有内部调用方，修改倍率或可用路由会立即影响它们全部的计费和可调用模型。",
                )
                .ok_text("确认修改")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let submit = submit.clone();
                    view.update(cx, |view, cx| view.submit(submit, cx));
                    true
                })
        });
    }

    fn submit(&mut self, submit: GroupSubmit, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.saving = true;
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let GroupSubmit {
                    editing,
                    name,
                    rate_bp,
                    routes,
                    name_changed,
                    rate_changed,
                    routes_changed,
                } = submit;
                match editing {
                    None => {
                        let group = UserGroupCreate {
                            name: &name,
                            rate_bp: Some(rate_bp),
                            route_aliases: routes.as_deref(),
                        };
                        admin.create_user_group(&group).await.map(|_| ())
                    }
                    Some(id) => {
                        let patch = UserGroupUpdate {
                            name: name_changed.then_some(name.as_str()),
                            rate_bp: rate_changed.then_some(rate_bp),
                            route_aliases: routes_changed.then_some(routes.as_deref()),
                        };
                        admin.patch_user_group(id, &patch).await.map(|_| ())
                    }
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |view, cx| {
                match result {
                    Ok(()) => {
                        view.editor = None;
                        view.reload(cx);
                        cx.emit(GroupsChanged);
                    }
                    Err(error) => {
                        if let Some(editor) = &mut view.editor {
                            editor.saving = false;
                            editor.error = Some(error);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Load the groups and the route aliases (a refresh keeps the old
    /// snapshot on screen).
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        crate::pages::begin_load(&mut self.state);
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move {
                let groups = admin.user_groups().await?;
                let aliases = admin
                    .routes()
                    .await?
                    .into_iter()
                    .map(|route| route.alias)
                    .collect();
                Ok(GroupSnapshot { groups, aliases })
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |view, cx| {
                view.state = match result {
                    Ok(snapshot) => LoadState::Loaded(snapshot),
                    Err(error) => LoadState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn editor_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        let editor = self.editor.as_ref()?;
        let aliases: &[String] = match &self.state {
            LoadState::Loaded(snapshot) => &snapshot.aliases,
            _ => &[],
        };
        let internal = editor.editing.is_some() && editor.original_name == INTERNAL_GROUP;
        let title = match (editor.editing, internal) {
            (None, _) => "新增用户组",
            (Some(_), true) => "编辑用户组（内部免费）",
            (Some(_), false) => "编辑用户组",
        };
        let saving = editor.saving;
        let mut form = card(cx)
            .gap_3()
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(title),
            )
            .child(editor_row("组名", Input::new(&editor.name), Some(220.), cx))
            .child(
                editor_row("倍率", Input::new(&editor.rate), Some(140.), cx).child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("× 金额，0 = 免费，最多 4 位小数，上限 100"),
                ),
            )
            .child(Self::route_picker(editor, aliases, cx));
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
                        Button::new("group-editor-save")
                            .primary()
                            .compact()
                            .label(if saving { "保存中…" } else { "保存" })
                            .disabled(saving)
                            .on_click(cx.listener(|view, _, window, cx| view.save(window, cx))),
                    )
                    .child(
                        Button::new("group-editor-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.editor = None;
                                cx.notify();
                            })),
                    ),
            ),
        )
    }

    /// 全部路由 plus one checkbox per alias (whitelisted aliases that are no
    /// longer routes stay listed so they can be unchecked).
    fn route_picker(editor: &GroupEditor, aliases: &[String], cx: &mut Context<Self>) -> Div {
        let all = editor.routes.is_none();
        let mut picker = gpui::div().flex().flex_wrap().items_center().gap_3().child(
            Checkbox::new("group-routes-all")
                .label("全部路由")
                .checked(all)
                .on_click(cx.listener(|view, checked: &bool, _, cx| {
                    if let Some(editor) = &mut view.editor {
                        // Unchecking 全部 starts from an empty whitelist.
                        editor.routes = if *checked { None } else { Some(Vec::new()) };
                    }
                    cx.notify();
                })),
        );
        let checked: &[String] = editor.routes.as_deref().unwrap_or(&[]);
        let stale = checked.iter().filter(|alias| !aliases.contains(alias));
        let listed = aliases.iter().map(|alias| (alias, false));
        let options = listed.chain(stale.map(|alias| (alias, true)));
        for (index, (alias, gone)) in options.enumerate() {
            let label = if gone {
                format!("{alias}（已删除）")
            } else {
                alias.clone()
            };
            let alias_owned = alias.clone();
            picker = picker.child(
                Checkbox::new(ElementId::NamedInteger("group-route".into(), index as u64))
                    .label(label)
                    .checked(all || checked.contains(alias))
                    .disabled(all)
                    .on_click(cx.listener(move |view, checked: &bool, _, cx| {
                        if let Some(editor) = &mut view.editor
                            && let Some(routes) = &mut editor.routes
                        {
                            routes.retain(|alias| *alias != alias_owned);
                            if *checked {
                                routes.push(alias_owned.clone());
                            }
                        }
                        cx.notify();
                    })),
            );
        }
        gpui::div()
            .flex()
            .items_start()
            .gap_2()
            .child(field_label("可用路由", cx))
            .child(gpui::div().flex_1().child(picker))
    }
}

fn group_table(groups: &[GroupRow], cx: &mut Context<UserGroupsView>) -> Div {
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
                .child(gpui::div().w(px(160.)).child("组名"))
                .child(gpui::div().w(px(72.)).child("倍率"))
                .child(gpui::div().flex_1().child("可用路由"))
                .child(gpui::div().w(px(56.)).child("用户数"))
                .child(gpui::div().w(px(40.)).child("操作")),
        );
    for group in groups {
        let id = group.id;
        let name = if group.name == INTERNAL_GROUP {
            format!("{}（内部免费）", group.name)
        } else {
            group.name.clone()
        };
        let routes = group
            .route_aliases
            .as_ref()
            .map_or_else(|| "全部".to_string(), |aliases| aliases.join(", "));
        table = table.child(
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
                        .w(px(160.))
                        .text_color(cx.theme().foreground)
                        .child(name),
                )
                .child(
                    gpui::div()
                        .w(px(72.))
                        .text_color(cx.theme().muted_foreground)
                        .child(gateway::billing::format_rate(group.rate_bp)),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_color(cx.theme().muted_foreground)
                        .child(routes),
                )
                .child(
                    gpui::div()
                        .w(px(56.))
                        .text_color(cx.theme().muted_foreground)
                        .child(group.user_count.to_string()),
                )
                .child(gpui::div().w(px(40.)).child(icon_button(
                    ElementId::NamedInteger("edit-group-btn".into(), group.id as u64),
                    IconName::Pencil,
                    "编辑",
                    cx.listener(move |view, _, window, cx| view.open_edit(id, window, cx)),
                ))),
        );
    }
    table
}

impl Render for UserGroupsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = gpui::div().flex().flex_col().gap_4();
        if let Some(editor) = self.editor_card(cx) {
            content = content.child(editor);
        }
        match &self.state {
            LoadState::Loading => content.child(loading_card(cx)),
            LoadState::Failed(error) => content.child(error_card(error, cx)),
            LoadState::Loaded(snapshot) if snapshot.groups.is_empty() => {
                content.child(empty_card("暂无用户组", "点右上角「新增用户组」创建。", cx))
            }
            LoadState::Loaded(snapshot) => content.child(group_table(&snapshot.groups, cx)),
        }
    }
}

/// A rate as the editor text: `format_rate` without the `×`.
pub(crate) fn rate_text(rate_bp: i64) -> String {
    let mut text = gateway::billing::format_rate(rate_bp);
    text.pop();
    text
}

/// Inline error of a rate that `billing::parse_rate` refused.
pub(crate) fn rate_error(text: &str) -> String {
    let trimmed = text.trim();
    // Same decimal grammar as the gateway, without the cap: tells "too big"
    // apart from "not a number".
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let over_cap = match trimmed.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction) && fraction.len() <= 4,
        None => digits(trimmed),
    };
    if over_cap {
        format!(
            "倍率不能超过 {}（上限 100×）",
            gateway::billing::format_rate(gateway::billing::MAX_RATE_BP)
        )
    } else {
        "倍率须为非负小数，最多 4 位小数，如 1 或 1.25（上限 100×）".to_string()
    }
}
