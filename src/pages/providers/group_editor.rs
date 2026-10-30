//! The two inline cards that edit account groups: 新建分组 (one name field)
//! and 管理分组 (one row per group: rename, delete). Both live above the
//! account cards, are mutually exclusive, and show every write failure
//! inside themselves — the server's message verbatim (409 duplicate name,
//! 400 invalid name) — never in the page-level `action_error`.

use std::collections::HashMap;

use gpui::{
    AppContext as _, Context, Div, Entity, Focusable as _, ParentElement, Styled, Window,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable};

use gateway::http_client::{ClientError, GatewayHttpClient};
use gateway::store::KeyGroupView;

use super::cards::{editor_field, inline_error};
use super::groups::{GroupFilter, counts};
use super::{LoadState, ProvidersData, ProvidersPage};
use crate::pages::card;
use crate::runtime::GatewayRuntime;

/// 新建分组 card state.
pub(super) struct GroupCreate {
    name: Entity<InputState>,
    /// The create call is in flight (double-click guard).
    saving: bool,
    error: Option<String>,
}

/// One row of the 管理分组 card.
struct GroupRow {
    name: Entity<InputState>,
    /// A rename or delete call for this row is in flight.
    saving: bool,
    error: Option<String>,
}

/// 管理分组 card state. Rows are keyed by group id and aligned with the
/// loaded groups at render time ([`ProvidersPage::sync_group_manager`]); an
/// existing row's input is never reset, so a reload cannot overwrite what the
/// user is typing.
pub(super) struct GroupManager {
    rows: HashMap<i64, GroupRow>,
    /// The group whose delete button is armed (「确认删除？」); any other
    /// button press disarms it.
    confirming_delete: Option<i64>,
}

/// Whether a row's 保存 is enabled: the typed name is non-empty after
/// trimming and differs from the stored one.
fn can_save(typed: &str, stored: &str) -> bool {
    let typed = typed.trim();
    !typed.is_empty() && typed != stored
}

impl ProvidersPage {
    /// 「＋ 新建分组」: open the create card (closing the manager).
    pub(super) fn begin_group_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.group_list_open = false;
        self.group_manager = None;
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("分组名称（最长 32 字符）"));
        let focus = name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.group_create = Some(GroupCreate {
            name,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    /// 「⚙ 管理分组…」: open the manager card (closing the create card).
    pub(super) fn begin_group_manage(&mut self, cx: &mut Context<Self>) {
        self.group_list_open = false;
        self.group_create = None;
        self.group_manager = Some(GroupManager {
            rows: HashMap::new(),
            confirming_delete: None,
        });
        cx.notify();
    }

    /// Align the manager's rows with the loaded groups: add a prefilled row
    /// for each new group, drop rows of vanished ones. Runs before each
    /// render (creating an input needs the window); a no-op when nothing
    /// changed.
    pub(super) fn sync_group_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(manager), LoadState::Loaded(data)) = (&mut self.group_manager, &self.state)
        else {
            return;
        };
        manager
            .rows
            .retain(|id, _| data.groups.iter().any(|group| group.id == *id));
        for group in &data.groups {
            // An existing row keeps its input untouched.
            manager.rows.entry(group.id).or_insert_with(|| GroupRow {
                name: cx.new(|cx| InputState::new(window, cx).default_value(group.name.as_str())),
                saving: false,
                error: None,
            });
        }
        if manager
            .confirming_delete
            .is_some_and(|id| !manager.rows.contains_key(&id))
        {
            manager.confirming_delete = None;
        }
    }

    /// 创建: create the group; on success close the card, reload and switch
    /// the filter to the new group. Until the reload lands the filter reads as
    /// 全部 (`GroupFilter::effective`: the id is not in the groups yet), then
    /// it resolves to the new group by itself.
    fn submit_group_create(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.group_create else {
            return;
        };
        if form.saving {
            return;
        }
        let name = form.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            form.error = Some("名称不能为空".to_string());
            cx.notify();
            return;
        }
        form.saving = true;
        form.error = None;
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.create_key_group(&name).await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(id) => {
                        page.group_create = None;
                        page.action_error = None;
                        page.group_filter = GroupFilter::Named(id);
                        page.reload(cx);
                    }
                    Err(error) => {
                        if let Some(form) = &mut page.group_create {
                            form.saving = false;
                            form.error = Some(error);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run one rename/delete write for a manager row: the row shows the
    /// in-flight state and, on failure, the server's message; success reloads.
    fn run_group_row_write<F, Fut>(&mut self, id: i64, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(GatewayHttpClient) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), ClientError>> + Send + 'static,
    {
        let Some(row) = self
            .group_manager
            .as_mut()
            .and_then(|manager| manager.rows.get_mut(&id))
        else {
            return;
        };
        if row.saving {
            return;
        }
        row.saving = true;
        row.error = None;
        cx.notify();
        let call = cx.global::<GatewayRuntime>().admin_call(f);
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let row = page
                    .group_manager
                    .as_mut()
                    .and_then(|manager| manager.rows.get_mut(&id));
                match result {
                    Ok(()) => {
                        if let Some(row) = row {
                            row.saving = false;
                        }
                        page.reload(cx);
                    }
                    Err(error) => {
                        if let Some(row) = row {
                            row.saving = false;
                            row.error = Some(error);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 保存: rename one group to the typed name.
    fn rename_group(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(manager) = &mut self.group_manager else {
            return;
        };
        manager.confirming_delete = None;
        let Some(row) = manager.rows.get(&id) else {
            return;
        };
        let name = row.name.read(cx).value().trim().to_string();
        self.run_group_row_write(id, cx, move |admin| async move {
            admin.rename_key_group(id, &name).await
        });
    }

    /// 删除: the first press arms the button, the second on the same row runs
    /// the delete.
    fn delete_group(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(manager) = &mut self.group_manager else {
            return;
        };
        if manager.confirming_delete != Some(id) {
            manager.confirming_delete = Some(id);
            cx.notify();
            return;
        }
        manager.confirming_delete = None;
        self.run_group_row_write(id, cx, move |admin| async move {
            admin.delete_key_group(id).await
        });
    }

    /// The 新建分组 card.
    pub(super) fn group_create_card(&self, form: &GroupCreate, cx: &mut Context<Self>) -> Div {
        card(cx)
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child("新建分组"),
            )
            .child(editor_field(
                "名称",
                Input::new(&form.name).disabled(form.saving),
                Some(240.),
                cx,
            ))
            .when_some(form.error.as_ref(), |this, error| {
                this.child(inline_error(error, cx))
            })
            .child(
                gpui::div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("group-create-cancel")
                            .label("取消")
                            .outline()
                            .compact()
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.group_create.as_ref().is_some_and(|form| !form.saving) {
                                    this.group_create = None;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("group-create-submit")
                            .label(if form.saving {
                                "创建中…"
                            } else {
                                "创建"
                            })
                            .primary()
                            .compact()
                            .loading(form.saving)
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_group_create(cx))),
                    ),
            )
    }

    /// The 管理分组 card. Rows come from `data.groups`; a group whose row has
    /// not been created yet (first frame after a reload) is skipped until the
    /// next sync.
    pub(super) fn group_manager_card(
        &self,
        manager: &GroupManager,
        data: &ProvidersData,
        cx: &mut Context<Self>,
    ) -> Div {
        let member_counts = counts(data.keys.iter().map(|card| card.key.id), &data.groups);
        let mut body = card(cx).child(
            gpui::div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    gpui::div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .child("管理分组"),
                )
                .child(
                    Button::new("group-manage-close")
                        .label("关闭")
                        .outline()
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.group_manager = None;
                            cx.notify();
                        })),
                ),
        );
        if data.groups.is_empty() {
            body = body.child(
                gpui::div()
                    .text_color(cx.theme().muted_foreground)
                    .child("尚无分组。可在顶栏「分组」下拉里新建。"),
            );
        }
        for (group, member_count) in data.groups.iter().zip(&member_counts.per_group) {
            if let Some(row) = manager.rows.get(&group.id) {
                body = body.child(self.group_row(
                    group,
                    row,
                    *member_count,
                    manager.confirming_delete == Some(group.id),
                    cx,
                ));
            }
        }
        body.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child("删除分组仅从各账号移除该分组，账号不受影响"),
        )
    }

    fn group_row(
        &self,
        group: &KeyGroupView,
        row: &GroupRow,
        member_count: usize,
        confirming: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let id = group.id;
        let typed = row.name.read(cx).value();
        let save_enabled = !row.saving && can_save(&typed, &group.name);
        let mut delete = Button::new(("group-row-delete", id as u64))
            .label(if confirming {
                "确认删除？"
            } else {
                "删除"
            })
            .compact()
            .disabled(row.saving)
            .on_click(cx.listener(move |this, _, _, cx| this.delete_group(id, cx)));
        delete = if confirming {
            delete.danger()
        } else {
            delete.outline()
        };
        gpui::div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::div()
                            .w(px(240.))
                            .child(Input::new(&row.name).disabled(row.saving)),
                    )
                    .child(
                        gpui::div()
                            .w(px(72.))
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{member_count} 个账号")),
                    )
                    .child(
                        Button::new(("group-row-save", id as u64))
                            .label("保存")
                            .primary()
                            .compact()
                            .loading(row.saving)
                            .disabled(!save_enabled)
                            .on_click(cx.listener(move |this, _, _, cx| this.rename_group(id, cx))),
                    )
                    .child(delete),
            )
            .when_some(row.error.as_ref(), |this, error| {
                this.child(inline_error(error, cx))
            })
    }
}
