//! Account groups on the keys page: the 分组 filter dropdown in the tab row and
//! the pure rules that decide which cards a filter admits. Membership lives
//! only on the group side (`KeyGroupView.key_ids`, pushed with the bootstrap),
//! so every answer here is derived from the loaded document at render time —
//! nothing is cached, nothing can drift from the data.

use std::collections::HashSet;

use gpui::{
    App, Context, Div, ElementId, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, prelude::FluentBuilder as _, px,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::Button;

use gateway::store::KeyGroupView;

use super::ProvidersPage;
use super::chip::selectable_chip;

/// Which accounts the page shows. Local view state: not persisted, and a
/// restart returns to [`GroupFilter::All`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupFilter {
    All,
    Ungrouped,
    Named(i64),
}

impl GroupFilter {
    /// The filter to apply to `groups`: a named group that no longer exists
    /// (deleted here or elsewhere) counts as 全部. Group ids are never reused,
    /// so a stale id left in the page state is harmless and needs no cleanup.
    pub(super) fn effective(self, groups: &[KeyGroupView]) -> Self {
        match self {
            Self::Named(id) if !groups.iter().any(|group| group.id == id) => Self::All,
            other => other,
        }
    }

    /// Empty-state copy (title, hint) for a filter that admits no account.
    pub(super) fn empty_copy(self) -> (&'static str, &'static str) {
        match self {
            Self::All => ("尚无账号", "点击「+ 添加账号」添加第一个上游账号。"),
            Self::Ungrouped => ("没有未分组的账号", "所有账号都已归入至少一个分组。"),
            Self::Named(_) => ("该分组暂无账号", "在账号的编辑面板里勾选分组。"),
        }
    }
}

/// A filter resolved against the current groups: the set of account ids the
/// filter keys on, copied out of the groups (ids are `Copy`, so it holds no
/// borrow). Built per render.
pub(super) struct Visibility {
    filter: GroupFilter,
    members: HashSet<i64>,
}

impl Visibility {
    pub(super) fn new(filter: GroupFilter, groups: &[KeyGroupView]) -> Self {
        let members = match filter {
            GroupFilter::All => HashSet::new(),
            GroupFilter::Ungrouped => groups
                .iter()
                .flat_map(|group| group.key_ids.iter().copied())
                .collect(),
            GroupFilter::Named(id) => groups
                .iter()
                .filter(|group| group.id == id)
                .flat_map(|group| group.key_ids.iter().copied())
                .collect(),
        };
        Self { filter, members }
    }

    /// Whether the account's card is shown.
    pub(super) fn admits(&self, key_id: i64) -> bool {
        match self.filter {
            GroupFilter::All => true,
            GroupFilter::Ungrouped => !self.members.contains(&key_id),
            GroupFilter::Named(_) => self.members.contains(&key_id),
        }
    }
}

/// Dropdown counts: accounts in total, accounts in no group, and per group
/// (same order as `groups`) the members that still exist in the account list.
pub(super) struct Counts {
    pub(super) all: usize,
    pub(super) ungrouped: usize,
    pub(super) per_group: Vec<usize>,
}

pub(super) fn counts(key_ids: impl Iterator<Item = i64>, groups: &[KeyGroupView]) -> Counts {
    let existing: HashSet<i64> = key_ids.collect();
    let mut grouped: HashSet<i64> = HashSet::new();
    let mut per_group = Vec::with_capacity(groups.len());
    for group in groups {
        let mut members = 0;
        for id in group.key_ids.iter().filter(|id| existing.contains(id)) {
            grouped.insert(*id);
            members += 1;
        }
        per_group.push(members);
    }
    Counts {
        all: existing.len(),
        ungrouped: existing.len() - grouped.len(),
        per_group,
    }
}

/// Ids of the groups the account belongs to, in `groups` order.
pub(super) fn memberships(groups: &[KeyGroupView], key_id: i64) -> Vec<i64> {
    groups
        .iter()
        .filter(|group| group.key_ids.contains(&key_id))
        .map(|group| group.id)
        .collect()
}

/// `ids` without the groups that no longer exist in `groups`.
pub(super) fn retain_existing(ids: &[i64], groups: &[KeyGroupView]) -> Vec<i64> {
    ids.iter()
        .copied()
        .filter(|id| groups.iter().any(|group| group.id == *id))
        .collect()
}

/// Whether the editor's group selection differs from what it opened with, as
/// sets: the order of clicking does not matter, so re-ticking a group that was
/// unticked does not count as a change (and costs no write on save).
pub(super) fn selection_changed(initial: &[i64], selected: &[i64]) -> bool {
    let initial: HashSet<i64> = initial.iter().copied().collect();
    let selected: HashSet<i64> = selected.iter().copied().collect();
    initial != selected
}

/// The key editor's 分组 row: every group as a toggle chip (highlighted when
/// the account is a member); a hint when no group exists yet.
pub(super) fn group_chips(
    selected: &[i64],
    groups: &[KeyGroupView],
    cx: &mut Context<ProvidersPage>,
) -> Div {
    if groups.is_empty() {
        return gpui::div()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child("尚无分组，可在顶栏「分组」里新建");
    }
    gpui::div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .children(groups.iter().map(|group| {
            let group_id = group.id;
            selectable_chip(
                group.name.clone(),
                selected.contains(&group_id),
                ElementId::from(("key-editor-group", group_id as u64)),
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_editor_group(group_id, cx)))
        }))
}

/// Group-name tags under a card's title; `None` when the account is in no
/// group. Computed from `groups` on every render, so it cannot drift from the
/// membership the gateway pushed. A long name truncates inside its own tag.
pub(super) fn group_tags(key_id: i64, groups: &[KeyGroupView], cx: &App) -> Option<Div> {
    let mut names = groups
        .iter()
        .filter(|group| group.key_ids.contains(&key_id))
        .map(|group| group.name.as_str())
        .peekable();
    names.peek()?;
    Some(
        gpui::div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(names.map(|name| {
                gpui::div()
                    .max_w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .rounded_md()
                    .border_1()
                    .px_1p5()
                    .text_size(px(10.))
                    .border_color(cx.theme().accent)
                    .text_color(cx.theme().muted_foreground)
                    .child(name.to_owned())
            })),
    )
}

impl ProvidersPage {
    /// The 分组 filter dropdown: the trigger names the current selection; the
    /// opened list floats above page content via `deferred` (never stretches
    /// the tab row). Only called once data has loaded.
    pub(super) fn group_dropdown(
        &self,
        data: &super::ProvidersData,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let filter = self.group_filter.effective(&data.groups);
        let current = match filter {
            GroupFilter::All => "全部",
            GroupFilter::Ungrouped => "未分组",
            GroupFilter::Named(id) => data
                .groups
                .iter()
                .find(|group| group.id == id)
                .map_or("全部", |group| group.name.as_str()),
        };
        let trigger = Button::new("providers-group-filter")
            .label(current)
            .outline()
            .compact()
            .on_click(cx.listener(|this, _, _, cx| {
                this.group_list_open = !this.group_list_open;
                cx.notify();
            }));
        if !self.group_list_open {
            return trigger.into_any_element();
        }
        let counts = counts(data.keys.iter().map(|card| card.key.id), &data.groups);
        let mut options = gpui::div()
            .id("providers-group-list")
            .flex()
            .flex_col()
            .border_1()
            .rounded_md()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .max_h(px(280.))
            .overflow_y_scroll()
            .child(self.filter_option(
                "providers-group-all",
                filter,
                GroupFilter::All,
                format!("全部 ({})", counts.all),
                cx,
            ))
            .child(self.filter_option(
                "providers-group-ungrouped",
                filter,
                GroupFilter::Ungrouped,
                format!("未分组 ({})", counts.ungrouped),
                cx,
            ));
        if !data.groups.is_empty() {
            options = options.child(separator(cx));
        }
        for (group, count) in data.groups.iter().zip(&counts.per_group) {
            options = options.child(self.filter_option(
                ("providers-group-named", group.id as u64),
                filter,
                GroupFilter::Named(group.id),
                format!("{} ({count})", group.name),
                cx,
            ));
        }
        options = options
            .child(separator(cx))
            .child(
                action_option("providers-group-create", "＋", "新建分组", cx).on_click(
                    cx.listener(|this, _, window, cx| this.begin_group_create(window, cx)),
                ),
            )
            .child(
                action_option("providers-group-manage", "⚙", "管理分组…", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.begin_group_manage(cx))),
            );
        // `absolute` takes the list out of document flow; `deferred` draws it
        // after all ancestors so it floats above page content. Anchored to the
        // trigger's right edge so it never runs past the window's right side.
        let floating = gpui::deferred(
            gpui::div()
                .absolute()
                .top(px(30.))
                .right(px(0.))
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

    /// One selectable filter line: selected = 「✓」 in a fixed-width lead slot + bold. Choosing it
    /// closes the list and re-filters locally (no reload).
    fn filter_option(
        &self,
        id: impl Into<ElementId>,
        current: GroupFilter,
        option: GroupFilter,
        text: String,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let selected = current == option;
        option_row(id, cx)
            .text_color(if selected {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            })
            .when(selected, |this| {
                this.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .flex()
            .items_center()
            .child(lead_slot(if selected { "✓" } else { "" }))
            .child(text)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.group_list_open = false;
                this.group_filter = option;
                cx.notify();
            }))
    }
}

/// A dropdown line that triggers an action instead of a selection.
fn action_option(
    id: impl Into<ElementId>,
    icon: &'static str,
    text: &'static str,
    cx: &mut Context<ProvidersPage>,
) -> gpui::Stateful<Div> {
    option_row(id, cx)
        .text_color(cx.theme().foreground)
        .flex()
        .items_center()
        .child(lead_slot(icon))
        .child(text)
}

/// Fixed-width lead cell shared by every dropdown line, so labels align
/// whether the cell holds a ✓, an action icon, or nothing.
fn lead_slot(mark: &'static str) -> Div {
    gpui::div().w(px(16.)).flex_none().child(mark)
}

/// Shared line chrome: padding, size, pointer and hover highlight.
fn option_row(id: impl Into<ElementId>, cx: &mut Context<ProvidersPage>) -> gpui::Stateful<Div> {
    gpui::div()
        .id(id)
        .cursor_pointer()
        .px_2()
        .py_1()
        .text_size(px(12.))
        .hover(|this| this.bg(cx.theme().accent))
}

fn separator(cx: &mut Context<ProvidersPage>) -> Div {
    gpui::div().h(px(1.)).my_1().bg(cx.theme().border)
}
