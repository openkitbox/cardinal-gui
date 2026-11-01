//! Account groups on the keys page: the 分组 filter dropdown in the tab row
//! (groups, providers, 归档) and the pure rules that decide which cards a
//! filter admits. Membership lives
//! only on the group side (`KeyGroupView.key_ids`, pushed with the bootstrap),
//! so every answer here is derived from the loaded document at render time —
//! nothing is cached, nothing can drift from the data.

use std::collections::{BTreeMap, HashSet};

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
    /// The accounts of one provider (by provider row id).
    Provider(i64),
    /// Archived accounts only; every other filter hides them.
    Archived,
}

/// The per-account facts the filter rules need, copied out of a card (all
/// `Copy`, so it holds no borrow).
#[derive(Clone, Copy)]
pub(super) struct KeyFacts {
    pub(super) id: i64,
    pub(super) provider_id: i64,
    pub(super) archived: bool,
}

impl GroupFilter {
    /// The filter to apply to `groups` and `keys`: a named group that no
    /// longer exists (deleted here or elsewhere), or a provider with no
    /// account left, counts as 全部. Ids are never reused, so a stale id left
    /// in the page state is harmless and needs no cleanup.
    pub(super) fn effective(
        self,
        groups: &[KeyGroupView],
        mut keys: impl Iterator<Item = KeyFacts>,
    ) -> Self {
        match self {
            Self::Named(id) if !groups.iter().any(|group| group.id == id) => Self::All,
            Self::Provider(id) if !keys.any(|key| key.provider_id == id) => Self::All,
            other => other,
        }
    }

    /// Empty-state copy (title, hint) for a filter that admits no account.
    pub(super) fn empty_copy(self) -> (&'static str, &'static str) {
        match self {
            Self::All => ("尚无账号", "点击「+ 添加账号」添加第一个上游账号。"),
            Self::Ungrouped => ("没有未分组的账号", "所有账号都已归入至少一个分组。"),
            Self::Named(_) => ("该分组暂无账号", "在账号的编辑面板里勾选分组。"),
            Self::Provider(_) => ("该供应商暂无未归档的账号", "它的账号都已归档。"),
            Self::Archived => ("没有已归档的账号", "在账号卡片的 ⋯ 菜单里选择「归档」。"),
        }
    }
}

/// A filter resolved against the current groups and accounts: the set of
/// account ids the filter keys on (group members, or a provider's accounts),
/// plus the ids of the archived accounts (ids are `Copy`, so it holds no
/// borrow). Built per render.
pub(super) struct Visibility {
    filter: GroupFilter,
    members: HashSet<i64>,
    archived: HashSet<i64>,
}

impl Visibility {
    pub(super) fn new(
        filter: GroupFilter,
        groups: &[KeyGroupView],
        keys: impl Iterator<Item = KeyFacts>,
    ) -> Self {
        let mut members = match filter {
            GroupFilter::All | GroupFilter::Provider(_) | GroupFilter::Archived => HashSet::new(),
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
        let mut archived = HashSet::new();
        for key in keys {
            if key.archived {
                archived.insert(key.id);
            }
            if filter == GroupFilter::Provider(key.provider_id) {
                members.insert(key.id);
            }
        }
        Self {
            filter,
            members,
            archived,
        }
    }

    /// Whether the account's card is shown. An archived account shows only
    /// under [`GroupFilter::Archived`], which shows nothing else.
    pub(super) fn admits(&self, key_id: i64) -> bool {
        let archived = self.archived.contains(&key_id);
        match self.filter {
            GroupFilter::Archived => archived,
            _ if archived => false,
            GroupFilter::All => true,
            GroupFilter::Ungrouped => !self.members.contains(&key_id),
            GroupFilter::Named(_) | GroupFilter::Provider(_) => self.members.contains(&key_id),
        }
    }
}

/// Dropdown counts over the unarchived accounts: in total, in no group, per
/// group (same order as `groups`) the members that still exist in the
/// account list, and per provider; plus the number of archived accounts.
pub(super) struct Counts {
    pub(super) all: usize,
    pub(super) ungrouped: usize,
    pub(super) per_group: Vec<usize>,
    /// Every provider that has an account (archived ones included, so the
    /// option stays listed), in provider id order.
    pub(super) per_provider: Vec<ProviderCount>,
    pub(super) archived: usize,
}

/// One provider line of the dropdown: its unarchived account count.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ProviderCount {
    pub(super) provider_id: i64,
    pub(super) count: usize,
}

pub(super) fn counts(keys: impl Iterator<Item = KeyFacts>, groups: &[KeyGroupView]) -> Counts {
    let mut existing: HashSet<i64> = HashSet::new();
    let mut archived = 0;
    let mut providers: BTreeMap<i64, usize> = BTreeMap::new();
    for key in keys {
        let count = providers.entry(key.provider_id).or_default();
        if key.archived {
            archived += 1;
        } else {
            existing.insert(key.id);
            *count += 1;
        }
    }
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
        per_provider: providers
            .into_iter()
            .map(|(provider_id, count)| ProviderCount { provider_id, count })
            .collect(),
        archived,
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

/// A 分组 row (key editor, Kiro import): every group as a toggle chip
/// (highlighted when selected); a hint when no group exists yet. `id_prefix`
/// keeps the chip ids of different forms apart; `toggle` is the page method
/// that flips one group in that form's selection.
pub(super) fn group_chips(
    selected: &[i64],
    groups: &[KeyGroupView],
    id_prefix: &'static str,
    toggle: fn(&mut ProvidersPage, i64, &mut Context<ProvidersPage>),
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
                ElementId::from((id_prefix, group_id as u64)),
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| toggle(this, group_id, cx)))
        }))
}

/// Group-name tags for a card's tag row (shared with the variant markers);
/// empty when the account is in no group. Computed from `groups` on every
/// render, so it cannot drift from the membership the gateway pushed. A long
/// name truncates inside its own tag.
pub(super) fn group_tags(key_id: i64, groups: &[KeyGroupView], cx: &App) -> Vec<Div> {
    groups
        .iter()
        .filter(|group| group.key_ids.contains(&key_id))
        .map(|group| {
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
                .child(group.name.clone())
        })
        .collect()
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
        let filter = self.group_filter.effective(&data.groups, data.key_facts());
        let current = match filter {
            GroupFilter::All => "全部",
            GroupFilter::Ungrouped => "未分组",
            GroupFilter::Named(id) => data
                .groups
                .iter()
                .find(|group| group.id == id)
                .map_or("全部", |group| group.name.as_str()),
            GroupFilter::Provider(id) => data.provider_name(id).unwrap_or("全部"),
            GroupFilter::Archived => "归档",
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
        let counts = counts(data.key_facts(), &data.groups);
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
        if !counts.per_provider.is_empty() {
            options = options.child(separator(cx));
        }
        for entry in &counts.per_provider {
            let name = data.provider_name(entry.provider_id).unwrap_or_default();
            options = options.child(self.filter_option(
                ("providers-group-provider", entry.provider_id as u64),
                filter,
                GroupFilter::Provider(entry.provider_id),
                format!("{name} ({})", entry.count),
                cx,
            ));
        }
        options = options
            .child(separator(cx))
            .child(self.filter_option(
                "providers-group-archived",
                filter,
                GroupFilter::Archived,
                format!("归档 ({})", counts.archived),
                cx,
            ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: i64, provider_id: i64, archived: bool) -> KeyFacts {
        KeyFacts {
            id,
            provider_id,
            archived,
        }
    }

    fn group(id: i64, key_ids: &[i64]) -> KeyGroupView {
        KeyGroupView {
            id,
            name: format!("g{id}"),
            key_ids: key_ids.to_vec(),
        }
    }

    #[test]
    fn provider_filter_admits_only_that_providers_unarchived_keys() {
        let keys = [key(1, 10, false), key(2, 20, false), key(3, 10, true)];
        let visibility = Visibility::new(GroupFilter::Provider(10), &[], keys.iter().copied());
        assert!(visibility.admits(1), "key 1 belongs to provider 10");
        assert!(!visibility.admits(2), "key 2 belongs to provider 20");
        assert!(!visibility.admits(3), "archived keys show only under 归档");
    }

    #[test]
    fn provider_filter_without_keys_falls_back_to_all() {
        let keys = [key(1, 10, false)];
        let filter = GroupFilter::Provider(99).effective(&[], keys.iter().copied());
        assert!(filter == GroupFilter::All, "provider 99 has no account");
        let kept = GroupFilter::Provider(10).effective(&[], keys.iter().copied());
        assert!(
            kept == GroupFilter::Provider(10),
            "provider 10 has an account"
        );
    }

    #[test]
    fn counts_per_provider_skip_archived_but_list_the_provider() {
        let keys = [
            key(1, 20, false),
            key(2, 10, false),
            key(3, 20, false),
            key(4, 30, true),
        ];
        let counts = counts(keys.iter().copied(), &[group(1, &[1])]);
        let per_provider: Vec<(i64, usize)> = counts
            .per_provider
            .iter()
            .map(|entry| (entry.provider_id, entry.count))
            .collect();
        assert_eq!(
            per_provider,
            [(10, 1), (20, 2), (30, 0)],
            "provider id order"
        );
        assert_eq!(counts.all, 3, "archived key 4 is excluded");
        assert_eq!(counts.ungrouped, 2, "only key 1 is grouped");
        assert_eq!(counts.archived, 1, "key 4 is archived");
    }
}
