//! Routes page: the live stage IS the route list — caller nodes → hub →
//! every route's entries stacked as sections in dispatch order, with dashed
//! wires flowing while a call is in flight. Management happens in manage
//! mode (tab-row 管理 toggle): inline ▲▼/✕ on entry rows, per-section
//! 编辑/删除 actions (the modal editor covers deep edits: labels, caps,
//! windows, context window, adding models). Read/write through the embedded
//! headless gateway.

mod stage;

use gpui::{
    App, AppContext as _, Context, Div, ElementId, Entity, Focusable as _, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Window,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable, WindowExt};

use gateway::admin_ops::Bootstrap;
use gateway::http_client::GatewayHttpClient;
use gateway::probe::ProbeModel;
use gateway::store::{ModelSuccessRow, RequestRow, RestRow, RouteEntry, RouteRow, RouteStrategy};

use std::collections::HashMap;

use crate::activity::{ActivityFeed, named_client};
use crate::pages::{
    LoadState, action_error_card, card, empty_card, error_card, loading_card, page_frame,
    refresh_button,
};
use crate::runtime::{GatewayRuntime, RelayMode};

use stage::StageClient;

/// How many recent requests the page keeps (the stage's resting source node
/// shows the latest one).
const RECENT_LIMIT: usize = 30;

/// The context-window choices the route editor offers, single-select.
/// Values are the vendors' declared numbers (OpenAI 128k, Anthropic 200k,
/// Kimi 256k, Gemini 1M).
const CONTEXT_PRESETS: [(i64, &str); 4] = [
    (128_000, "128k"),
    (200_000, "200k"),
    (262_144, "256k"),
    (1_048_576, "1M"),
];

/// One key as the route editor sees it: identity plus display labels.
struct RouteKey {
    id: i64,
    provider_name: String,
    label: String,
    position: i32,
    /// Manually disabled on the accounts page — the stage marks the entry
    /// (dispatch skips disabled keys just like cooled ones).
    enabled: bool,
    /// The key's model whitelist (None/empty = unrestricted). The picker hides
    /// chips outside it — a route can only reference models the account serves.
    allowed_models: Option<Vec<String>>,
}

/// Loaded page data: routes plus every key in global position order, and the
/// recent requests the route cards'「最近调用」line reads.
struct RoutesData {
    routes: Vec<RouteRow>,
    keys: Vec<RouteKey>,
    /// Recent requests, newest first (capped at [`RECENT_LIMIT`]).
    recent: Vec<RequestRow>,
    /// Latest successful request time per (key, served model) — the stage's
    /// "上次成功" markers on idle entries.
    success: Vec<ModelSuccessRow>,
    /// Active cooldowns — the stage marks cooled (key, model) entries.
    rests: Vec<RestRow>,
}

impl RoutesData {
    /// Project the pushed bootstrap document into this page's shape, plus the
    /// live ledger reads it alone needs (the document deliberately carries no
    /// request history — those are parameterless but unbounded).
    fn from_bootstrap(
        document: &Bootstrap,
        recent: Vec<RequestRow>,
        success: Vec<ModelSuccessRow>,
    ) -> Self {
        let providers: HashMap<i64, &str> = document
            .providers
            .iter()
            .map(|row| (row.id, row.name.as_str()))
            .collect();
        let mut keys: Vec<RouteKey> = document
            .keys
            .iter()
            .filter_map(|key| {
                let provider_name = providers.get(&key.provider_id)?;
                Some(RouteKey {
                    id: key.id,
                    provider_name: (*provider_name).to_string(),
                    label: key.label.clone(),
                    position: key.position,
                    enabled: key.enabled,
                    allowed_models: key.allowed_models.clone(),
                })
            })
            .collect();
        keys.sort_by(|a, b| a.position.cmp(&b.position).then(a.id.cmp(&b.id)));
        Self {
            routes: document.routes.clone(),
            keys,
            recent,
            success,
            rests: document.rests.clone(),
        }
    }
}

/// One availability rule being edited: an optional time window + optional
/// weekday restriction + allow/deny effect. Both time inputs empty = the
/// whole day (meaningful only with days selected); both empty AND no days =
/// always (a catch-all rule).
struct RuleDraft {
    /// Rule effect: true = allow, false = deny (关闭).
    allow: bool,
    /// "HH:MM" each; both empty = whole day.
    window_start: Entity<InputState>,
    window_end: Entity<InputState>,
    /// Weekday restriction (ISO 1=Mon..7=Sun); empty = every day.
    days: Vec<u8>,
}

/// One route item being edited: a model (via one key) with its optional
/// label, per-item concurrency cap, and an ordered availability-rule list
/// (first match wins; an empty list = always available). The route is a FLAT
/// list of these — the same key may appear in several items, one model each.
struct ItemDraft {
    key_id: i64,
    model: String,
    label: Entity<InputState>,
    /// Per-item concurrency cap (empty = unlimited at the item level; the
    /// key-level cap still applies on top).
    cap: Entity<InputState>,
    /// Ordered availability rules (top = highest priority).
    rules: Vec<RuleDraft>,
}

/// One picker row in the route editor: an on-demand「账号 → 模型」selector.
/// Clicking ＋添加模型 appends a row; the account button opens an inline
/// dropdown of accounts, choosing one fetches its models as toggle chips
/// (click = add/remove an item in the flat list). Rows are throwaway
/// pickers — they never own route state, the flat `items` list does.
struct PickerRow {
    /// The chosen account (None until the dropdown selection is made).
    key_id: Option<i64>,
    /// Whether the inline account dropdown is open.
    account_list_open: bool,
    /// Fetched models for the chosen account; None until asked, cached
    /// afterwards (a `Failed` is re-asked via 重试).
    models: Option<LoadState<Vec<ProbeModel>>>,
}

/// Route create/edit card state. `original_alias` is `Some` when editing (the
/// alias is the primary key, so it is read-only then).
struct RouteEditor {
    original_alias: Option<String>,
    alias: Entity<InputState>,
    /// Declared context window in tokens (None = undeclared); single-select
    /// among [`CONTEXT_PRESETS`], served to clients on /v1/models.
    context_window: Option<i64>,
    /// Dispatch strategy: ordered (list order) or smart (quota-aware). Set by
    /// the strategy picker; a new route defaults to `ordered`.
    strategy: RouteStrategy,
    /// The route itself: a flat model list, in append order.
    items: Vec<ItemDraft>,
    /// On-demand picker rows (＋添加模型 appends one) — the account list is
    /// never rendered flat, so dozens of accounts don't blow up the card.
    rows: Vec<PickerRow>,
    error: Option<String>,
}

/// Snapshot of one availability rule, detached from the input entities.
struct RuleSelection {
    allow: bool,
    window_start: String,
    window_end: String,
    days: Vec<u8>,
}

/// Snapshot of one route item, detached from the input entities.
struct SelectionDraft {
    key_id: i64,
    model: String,
    label: String,
    cap: String,
    rules: Vec<RuleSelection>,
}

/// Editor state → route entries: one entry per item, in list order (empty
/// label/cap mean "use the model id" / unlimited; an empty rule list =
/// always available).
fn route_entries(drafts: &[SelectionDraft]) -> Result<Vec<RouteEntry>, String> {
    if drafts.is_empty() {
        return Err("至少添加一个模型".to_string());
    }
    let mut entries = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let max_concurrency = match draft.cap.as_str() {
            "" => None,
            cap => match cap.parse::<i64>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Err(format!("{} 的并发上限必须是正整数", draft.model)),
            },
        };
        let mut rules = Vec::with_capacity(draft.rules.len());
        for (index, rule_draft) in draft.rules.iter().enumerate() {
            let days = if rule_draft.days.is_empty() {
                None
            } else {
                Some(rule_draft.days.clone())
            };
            let window = match (
                rule_draft.window_start.is_empty(),
                rule_draft.window_end.is_empty(),
                &days,
            ) {
                (true, true, None) => {
                    // Catch-all (always matches): stored as a whole-day window.
                    gateway::store::ActiveWindow {
                        start: "00:00".into(),
                        end: "00:00".into(),
                        days: Some(vec![1, 2, 3, 4, 5, 6, 7]),
                    }
                }
                (true, true, days @ Some(_)) => {
                    // Days-only restriction, no time bounds: the full day.
                    gateway::store::ActiveWindow {
                        start: "00:00".into(),
                        end: "00:00".into(),
                        days: days.clone(),
                    }
                }
                (false, false, days) => {
                    let window = gateway::store::ActiveWindow {
                        start: rule_draft.window_start.clone(),
                        end: rule_draft.window_end.clone(),
                        days: days.clone(),
                    };
                    window.validate().map_err(|_| {
                        format!(
                            "{} 规则 {} 的时段必须是 HH:MM 且起止不同",
                            draft.model,
                            index + 1
                        )
                    })?;
                    window
                }
                _ => {
                    return Err(format!(
                        "{} 规则 {} 的时段要成对填写（起与止）",
                        draft.model,
                        index + 1
                    ));
                }
            };
            let effect = if rule_draft.allow {
                gateway::store::RuleEffect::Allow
            } else {
                gateway::store::RuleEffect::Deny
            };
            rules.push(gateway::store::AvailabilityRule { window, effect });
        }
        let label = if draft.label.is_empty() {
            None
        } else {
            Some(draft.label.clone())
        };
        entries.push(RouteEntry {
            key_id: draft.key_id,
            model: draft.model.clone(),
            label,
            max_concurrency,
            rules,
            active_window: None,
        });
    }
    Ok(entries)
}

/// Fresh inputs for one availability rule (default = allow, whole day).
fn rule_draft(window: &mut Window, cx: &mut App) -> RuleDraft {
    let window_start = cx.new(|cx| InputState::new(window, cx).placeholder("起 08:00"));
    let window_end = cx.new(|cx| InputState::new(window, cx).placeholder("止 20:00"));
    RuleDraft {
        allow: true,
        window_start,
        window_end,
        days: Vec::new(),
    }
}

/// Fresh inputs for one route item (defaults; editing pre-fills afterwards).
fn item_draft(key_id: i64, model: String, window: &mut Window, cx: &mut App) -> ItemDraft {
    let label = cx.new(|cx| InputState::new(window, cx).placeholder("空 = 模型 ID"));
    let cap = cx.new(|cx| InputState::new(window, cx).placeholder("不限"));
    ItemDraft {
        key_id,
        model,
        label,
        cap,
        rules: Vec::new(),
    }
}

/// Route alias management: the live stage shows every route as a section
/// (caller nodes → hub → entries); manage mode adds inline entry reorder/
/// delete and per-section edit/delete actions, with the modal editor for
/// creating routes and deep edits.
pub struct RoutesPage {
    state: LoadState<RoutesData>,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    editor: Option<RouteEditor>,
    /// Failure of a manage write (entry move/remove, route delete); shown
    /// above the stage.
    action_error: Option<String>,
    /// Manage mode: the stage's sections carry edit/delete actions and each
    /// entry row its ▲▼/✕ (tab-row 管理/完成 toggle).
    manage: bool,
    /// A manage write (entry move/remove) is in flight; the manage controls
    /// stay disabled until it lands, so rapid clicks can't compute competing
    /// full-entry PUTs off the same stale snapshot (last-write-wins would
    /// silently reorder).
    manage_write_in_flight: bool,
    /// The shared live-activity feed driving the stage (observed).
    feed: Entity<ActivityFeed>,
    /// Persisted counter already handled (debounced reload on change).
    seen_persisted: u64,
    /// A debounced reload is scheduled (coalesces a settle storm).
    refresh_pending: bool,
    /// Listen address shown under the hub node ("" until resolved / Down).
    hub_addr: String,
    /// Flow phase of the live wires' dash pattern, 0..8 (3 on + 5 off).
    phase: f32,
    /// The flow ticker is armed (one chain at a time, self-extends while live).
    anim_armed: bool,
    /// Client nodes of the stage (derived from caller labels, e.g.
    /// `cardinal-opencode` → `opencode`): live while a call is in flight,
    /// lingering CLIENT_LINGER after the last one, then dropped.
    clients: Vec<StageClient>,
    /// Stale-prune-timer guard: bumping invalidates timers armed against an
    /// older registry.
    clients_gen: u64,
    /// The deadline the current prune timer is armed for; re-arming for the
    /// same deadline keeps the existing timer (no one task per feed event).
    prune_armed_until: Option<std::time::Instant>,
}

/// A client node lingers this long after its last request, then leaves the
/// stage. (The stage doubles as a "who is connected" view, so clients stay
/// much longer than a request-scoped one would keep them.)
impl RoutesPage {
    pub fn new(feed: Entity<ActivityFeed>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: LoadState::Loading,
            generation: 0,
            editor: None,
            action_error: None,
            manage: false,
            manage_write_in_flight: false,
            feed,
            seen_persisted: 0,
            refresh_pending: false,
            hub_addr: String::new(),
            phase: 0.0,
            anim_armed: false,
            clients: Vec::new(),
            clients_gen: 0,
            prune_armed_until: None,
        };
        // The hub node's subtitle: the relay's listen address, resolved once.
        let relay_mode = cx.global::<GatewayRuntime>().relay_mode();
        cx.spawn(async move |this, cx| {
            if let Ok(RelayMode::Connected(addr)) = relay_mode.await {
                this.update(cx, |page, cx| {
                    page.hub_addr = addr;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        // Any activity event folds into the client registry and repaints the
        // stage; Persisted also schedules a debounced reload (the stage's
        // sections and the client seeding read the ledger). The flow ticker arms
        // while anything is in flight.
        // No eager load here: the page opens (and re-opens) on a `resync` —
        // bumped by the activity stream's connect frame. That keeps opening
        // the client cheap (the feed subscription carries the bootstrap, and
        // the stage's first snapshot loads when the stream is up).
        let feed = this.feed.clone();
        cx.observe(&feed, |page, feed, cx| {
            // Snapshot the live set first: folding it into the registry needs
            // &mut Context (arm_prune spawns), which conflicts with feed.read(cx).
            let live_calls: Vec<(String, String)> = feed
                .read(cx)
                .live()
                .map(|attempt| {
                    (
                        named_client(attempt.agent.as_deref(), attempt.caller.as_deref()),
                        attempt.client_model.clone(),
                    )
                })
                .collect();
            let idle = live_calls.is_empty();
            page.sync_clients(live_calls, cx);
            if !idle {
                page.arm_anim(cx);
            }
            let persisted = feed.read(cx).persisted();
            if persisted != page.seen_persisted && !page.refresh_pending {
                page.seen_persisted = persisted;
                page.refresh_pending = true;
                cx.spawn(async move |page, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    page.update(cx, |page, cx| {
                        page.refresh_pending = false;
                        // Absorb every Persisted that arrived during the debounce, or
                        // the next event schedules one redundant reload.
                        page.seen_persisted = page.feed.read(cx).persisted();
                        page.reload(cx);
                    })
                    .ok();
                })
                .detach();
            }
            cx.notify();
        })
        .detach();
        // The connect frame's resync (bumped along with the pushed bootstrap)
        // may have landed before this page existed: load its first snapshot
        // now. A later connect fires the observer, so every (re)connect still
        // reloads.
        let resync = this.feed.read(cx).resync();
        if matches!(this.state, LoadState::Loading) && resync > 0 {
            this.reload(cx);
        }
        this
    }

    /// Tab-row actions (rendered by the Status container): refresh always;
    /// 管理 toggle and 添加路由 only once data has loaded, since the stage and
    /// the editor need the route/key lists.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "routes-refresh",
            cx.listener(|this, _, _, cx| this.reload(cx)),
        );
        match &self.state {
            LoadState::Loaded(_) => vec![
                Button::new("routes-manage")
                    .label(if self.manage { "完成" } else { "管理" })
                    .outline()
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.manage = !this.manage;
                        cx.notify();
                    }))
                    .into_any_element(),
                Button::new("route-add")
                    .icon(Icon::new(IconName::Plus).small())
                    .label("添加路由")
                    .outline()
                    .compact()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_route_editor(None, window, cx)),
                    )
                    .into_any_element(),
                refresh.into_any_element(),
            ],
            _ => vec![refresh.into_any_element()],
        }
    }

    /// (Re)load the page: the pushed bootstrap document's slice (routes, every
    /// key, cooldowns) plus the two live ledger reads it alone needs (recent
    /// requests, model-success). ONE bootstrap call replaces the per-provider
    /// key fan-out this page used to make.
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        crate::pages::begin_load(&mut self.state);
        self.generation += 1;
        let generation = self.generation;
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move {
                let document = admin.bootstrap().await?;
                let recent = admin.recent_requests(RECENT_LIMIT as i64, None).await?;
                let success = admin.model_success().await?;
                Ok(RoutesData::from_bootstrap(&document, recent, success))
            });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // a newer reload is in flight; this snapshot is stale
                }
                page.state = match result {
                    Ok(data) => LoadState::Loaded(data),
                    Err(error) => LoadState::Failed(error),
                };
                page.seed_clients();
                page.arm_prune(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the route editor: blank for creating, prefilled when editing.
    fn open_route_editor(
        &mut self,
        existing: Option<RouteRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let LoadState::Loaded(_) = &self.state else {
            return;
        };
        let alias = cx.new(|cx| InputState::new(window, cx).placeholder("对外模型 ID（如 smart）"));
        let items: Vec<ItemDraft> = existing
            .as_ref()
            .map(|route| {
                route
                    .entries
                    .iter()
                    .map(|entry| {
                        let mut draft = item_draft(entry.key_id, entry.model.clone(), window, cx);
                        if let Some(text) = &entry.label {
                            draft
                                .label
                                .update(cx, |input, cx| input.set_value(text.clone(), window, cx));
                        }
                        if let Some(cap) = entry.max_concurrency {
                            draft.cap.update(cx, |input, cx| {
                                input.set_value(cap.to_string(), window, cx)
                            });
                        }
                        for rule in entry.rules() {
                            let mut draft_rule = rule_draft(window, cx);
                            draft_rule.allow = rule.effect == gateway::store::RuleEffect::Allow;
                            draft_rule.window_start.update(cx, |input, cx| {
                                input.set_value(rule.window.start.clone(), window, cx)
                            });
                            draft_rule.window_end.update(cx, |input, cx| {
                                input.set_value(rule.window.end.clone(), window, cx)
                            });
                            if let Some(days) = &rule.window.days {
                                draft_rule.days = days.clone();
                            }
                            draft.rules.push(draft_rule);
                        }
                        draft
                    })
                    .collect()
            })
            .unwrap_or_default();
        let context_window = existing.as_ref().and_then(|route| route.context_window);
        let strategy = existing
            .as_ref()
            .map(|route| route.strategy)
            .unwrap_or_default();
        if let Some(route) = &existing {
            alias.update(cx, |input, cx| {
                input.set_value(route.alias.clone(), window, cx)
            });
        }
        let focus = alias.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.editor = Some(RouteEditor {
            original_alias: existing.map(|route| route.alias),
            alias,
            context_window,
            strategy,
            items,
            rows: Vec::new(),
            error: None,
        });
        cx.notify();
    }

    /// Append one empty picker row (the ＋添加模型 button).
    fn add_picker_row(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        editor.rows.push(PickerRow {
            key_id: None,
            account_list_open: true,
            models: None,
        });
        cx.notify();
    }

    /// Remove one picker row (its ✕). Items already added via the row stay.
    fn remove_picker_row(&mut self, row_index: usize, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if row_index < editor.rows.len() {
            editor.rows.remove(row_index);
        }
        cx.notify();
    }

    /// Open/close a row's inline account dropdown.
    fn toggle_account_list(&mut self, row_index: usize, open: bool, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(row) = editor.rows.get_mut(row_index) else {
            return;
        };
        row.account_list_open = open;
        cx.notify();
    }

    /// Pick an account for a row: closes the dropdown and starts the model
    /// fetch. Re-picking another account refetches.
    fn select_row_account(&mut self, row_index: usize, key_id: i64, cx: &mut Context<Self>) {
        {
            let Some(editor) = &mut self.editor else {
                return;
            };
            let Some(row) = editor.rows.get_mut(row_index) else {
                return;
            };
            if row.key_id == Some(key_id) {
                row.account_list_open = false;
                cx.notify();
                return;
            }
            row.key_id = Some(key_id);
            row.account_list_open = false;
            row.models = None;
        }
        self.fetch_row_models(row_index, cx);
        cx.notify();
    }

    /// Toggle one model chip of a key: a chip corresponds to the (key, model)
    /// item in the flat route list — clicking adds it, clicking again removes
    /// it. The same key can supply several items, one model each.
    fn toggle_model(
        &mut self,
        key_id: i64,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if let Some(index) = editor
            .items
            .iter()
            .position(|item| item.key_id == key_id && item.model == model)
        {
            editor.items.remove(index);
        } else {
            editor.items.push(item_draft(key_id, model, window, cx));
        }
        cx.notify();
    }

    /// Fetch the chosen account's upstream model list into the row (skipped
    /// while a fetch is in flight or already cached; a failure is re-asked).
    fn fetch_row_models(&mut self, row_index: usize, cx: &mut Context<Self>) {
        let Some(key_id) = ({
            let Some(editor) = &mut self.editor else {
                return;
            };
            let Some(row) = editor.rows.get_mut(row_index) else {
                return;
            };
            if matches!(row.models, Some(LoadState::Loading | LoadState::Loaded(_))) {
                return;
            }
            row.models = Some(LoadState::Loading);
            row.key_id
        }) else {
            return;
        };
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.list_key_models(key_id).await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                if let Some(editor) = &mut page.editor
                    && let Some(row) = editor.rows.get_mut(row_index)
                    && row.key_id == Some(key_id)
                {
                    row.models = Some(match result {
                        Ok(models) => LoadState::Loaded(models),
                        Err(error) => LoadState::Failed(error),
                    });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Re-ask the model list for the row currently bound to `key_id`
    /// (the 重试 button carries the account id, not a row index).
    fn retry_key_models(&mut self, key_id: i64, cx: &mut Context<Self>) {
        let row_index = self.editor.as_ref().and_then(|editor| {
            editor
                .rows
                .iter()
                .position(|row| row.key_id == Some(key_id))
        });
        let Some(row_index) = row_index else { return };
        if let Some(editor) = &mut self.editor
            && let Some(row) = editor.rows.get_mut(row_index)
        {
            row.models = None;
        }
        self.fetch_row_models(row_index, cx);
    }

    /// Save the route editor, then reload. Validation errors stay in the card.
    /// Shares the manage-write serialization flag: an editor save PUTs a full
    /// entry snapshot too, so it must not race an in-flight ▲▼/✕ write.
    fn save_route(&mut self, cx: &mut Context<Self>) {
        if self.manage_write_in_flight {
            return;
        }
        let Some(editor) = &self.editor else { return };
        let alias = editor.alias.read(cx).value().trim().to_string();
        let context_window = editor.context_window;
        let strategy = editor.strategy;
        let drafts: Vec<SelectionDraft> = editor
            .items
            .iter()
            .map(|item| SelectionDraft {
                key_id: item.key_id,
                model: item.model.clone(),
                label: item.label.read(cx).value().trim().to_string(),
                cap: item.cap.read(cx).value().trim().to_string(),
                rules: item
                    .rules
                    .iter()
                    .map(|rule| RuleSelection {
                        allow: rule.allow,
                        window_start: rule.window_start.read(cx).value().trim().to_string(),
                        window_end: rule.window_end.read(cx).value().trim().to_string(),
                        days: rule.days.clone(),
                    })
                    .collect(),
            })
            .collect();
        let entries = if alias.is_empty() {
            Err("别名不能为空".to_string())
        } else {
            route_entries(&drafts)
        };
        let entries = match entries {
            Ok(entries) => entries,
            Err(message) => {
                if let Some(editor) = &mut self.editor {
                    editor.error = Some(message);
                }
                cx.notify();
                return;
            }
        };
        // The editor carries the picked strategy; the save sends it, so an
        // untouched editor round-trips the route's stored value.
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                admin
                    .put_route(&alias, &entries, context_window, strategy)
                    .await
            });
        self.manage_write_in_flight = true;
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.manage_write_in_flight = false;
                match result {
                    Ok(()) => {
                        page.editor = None;
                        page.action_error = None;
                        page.reload(cx);
                    }
                    Err(error) => {
                        if let Some(editor) = &mut page.editor {
                            editor.error = Some(error);
                        } else {
                            page.action_error = Some(error);
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Delete-confirm dialog for a route (gpui-kit AlertDialog confirm).
    fn confirm_delete_route(&mut self, alias: String, window: &mut Window, cx: &mut Context<Self>) {
        let page = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let on_ok_page = page.clone();
            let on_ok_alias = alias.clone();
            dialog
                .confirm()
                .title("删除路由?")
                .description(format!("模型 ID「{alias}」将不再可用。"))
                .ok_text("删除")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let alias = on_ok_alias.clone();
                    on_ok_page.update(cx, |page, cx| {
                        page.manage_write(cx, move |admin| async move {
                            admin.delete_route(&alias).await
                        })
                    });
                    true
                })
        });
    }

    /// Move one entry within its route (manage-mode ▲▼): the stored order IS
    /// the dispatch order, so this re-PUTs the route with two entries swapped.
    fn move_entry(&mut self, alias: String, index: usize, up: bool, cx: &mut Context<Self>) {
        if self.manage_write_in_flight {
            return;
        }
        let LoadState::Loaded(data) = &self.state else {
            return;
        };
        let Some(route) = data.routes.iter().find(|route| route.alias == alias) else {
            return;
        };
        let mut entries = route.entries.clone();
        // The index comes from a rendered frame; a debounced reload may have
        // shrunk the route since — never trust it against the fresh snapshot.
        if index >= entries.len() {
            return;
        }
        let target = if up {
            index.checked_sub(1)
        } else {
            (index + 1 < entries.len()).then_some(index + 1)
        };
        let Some(target) = target else { return };
        entries.swap(index, target);
        let context_window = route.context_window;
        let strategy = route.strategy;
        self.manage_write(cx, move |admin| async move {
            admin
                .put_route(&alias, &entries, context_window, strategy)
                .await
        });
    }

    /// Delete-confirm dialog for one entry (manage-mode ✕): re-PUTs the route
    /// without it. The store requires ≥1 entry per route, so the last row's ✕
    /// is disabled — removing the whole route is the section-level 删除 action.
    fn confirm_remove_entry(
        &mut self,
        alias: String,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.manage_write_in_flight {
            return;
        }
        let LoadState::Loaded(data) = &self.state else {
            return;
        };
        let Some(route) = data.routes.iter().find(|route| route.alias == alias) else {
            return;
        };
        let Some(entry) = route.entries.get(index) else {
            return;
        };
        let model = entry.label.clone().unwrap_or_else(|| entry.model.clone());
        let mut entries = route.entries.clone();
        entries.remove(index);
        let context_window = route.context_window;
        let strategy = route.strategy;
        let page = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let on_ok_page = page.clone();
            let on_ok_alias = alias.clone();
            let on_ok_entries = entries.clone();
            dialog
                .confirm()
                .title("移除模型?")
                .description(format!("路由「{alias}」将不再派发 {model}。"))
                .ok_text("移除")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let alias = on_ok_alias.clone();
                    let entries = on_ok_entries.clone();
                    on_ok_page.update(cx, |page, cx| {
                        page.manage_write(cx, move |admin| async move {
                            admin
                                .put_route(&alias, &entries, context_window, strategy)
                                .await
                        })
                    });
                    true
                })
        });
    }

    /// One manage-mode write with in-flight serialization (see
    /// `manage_write_in_flight`); on success clear the error and reload, on
    /// failure show the message — same contract as `run_write`, plus the flag.
    fn manage_write<F, Fut>(&mut self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(GatewayHttpClient) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), gateway::http_client::ClientError>>
            + Send
            + 'static,
    {
        // Defense in depth: entry points check the flag, but so do we — a future
        // caller must not bypass the serialization.
        if self.manage_write_in_flight {
            return;
        }
        self.manage_write_in_flight = true;
        let call = cx.global::<GatewayRuntime>().admin_call(f);
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.manage_write_in_flight = false;
                match result {
                    Ok(()) => {
                        page.action_error = None;
                        page.reload(cx);
                    }
                    Err(error) => {
                        page.action_error = Some(error);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Route editor card: alias and context window over the flat item list,
    /// with per-provider key pickers (expandable rows of model chips) feeding it.
    fn route_editor_card(
        &self,
        editor: &RouteEditor,
        data: &RoutesData,
        cx: &mut Context<Self>,
    ) -> Div {
        let title = match &editor.original_alias {
            Some(alias) => format!("编辑路由 {alias}"),
            None => "添加路由".to_string(),
        };
        let editing = editor.original_alias.is_some();
        let item_count = editor.items.len();
        card(cx)
      .child(gpui::div().font_weight(gpui::FontWeight::MEDIUM).text_color(cx.theme().foreground).child(title))
      .child(
        gpui::div()
          .flex()
          .items_center()
          .gap_2()
          .child(gpui::div().w(px(72.)).flex_shrink_0().text_color(cx.theme().muted_foreground).child("别名"))
          .child(gpui::div().flex_1().child(Input::new(&editor.alias).w_full().disabled(editing))),
      )
      .child(
        gpui::div()
          .flex()
          .items_center()
          .gap_2()
          .child(gpui::div().w(px(72.)).flex_shrink_0().text_color(cx.theme().muted_foreground).child("最大上下文"))
          .children(CONTEXT_PRESETS.into_iter().map(|(value, label)| {
            let selected = editor.context_window == Some(value);
            let chip = gpui::div()
              .id(ElementId::from(gpui::SharedString::from(format!("route-context-{value}"))))
              .cursor_pointer()
              .border_1()
              .rounded_md()
              .px_2()
              .py_0p5()
              .text_size(px(12.))
              .child(label);
            let chip = if selected {
              chip.border_color(cx.theme().accent).bg(cx.theme().accent).text_color(cx.theme().accent_foreground)
            } else {
              chip.border_color(cx.theme().border).text_color(cx.theme().foreground).hover(|this| this.bg(cx.theme().accent))
            };
            // Single-select; clicking the selected one clears back to 不声明.
            chip.on_click(cx.listener(move |this, _, _, cx| {
              if let Some(editor) = &mut this.editor {
                editor.context_window = if selected { None } else { Some(value) };
              }
              cx.notify();
            }))
          }))
          .child(
            gpui::div()
              .text_size(px(12.))
              .text_color(cx.theme().muted_foreground)
              .child("单选；不选 = 不声明。客户端在 /v1/models 里读到，用于规划压缩"),
          ),
      )
      .child(
        gpui::div()
          .flex()
          .items_center()
          .gap_2()
          .child(gpui::div().w(px(72.)).flex_shrink_0().text_color(cx.theme().muted_foreground).child("策略"))
          .children([RouteStrategy::Ordered, RouteStrategy::Smart, RouteStrategy::Rotate].into_iter().map(|strategy| {
            let selected = editor.strategy == strategy;
            let label = match strategy {
              RouteStrategy::Ordered => "顺序",
              RouteStrategy::Smart => "智能",
              RouteStrategy::Rotate => "交替",
            };
            let chip = gpui::div()
              .id(ElementId::from(gpui::SharedString::from(format!("route-strategy-{label}"))))
              .cursor_pointer()
              .border_1()
              .rounded_md()
              .px_2()
              .py_0p5()
              .text_size(px(12.))
              .child(label);
            let chip = if selected {
              chip.border_color(cx.theme().accent).bg(cx.theme().accent).text_color(cx.theme().accent_foreground)
            } else {
              chip.border_color(cx.theme().border).text_color(cx.theme().foreground).hover(|this| this.bg(cx.theme().accent))
            };
            // Single-select: 顺序 dispatches in list order, 智能 picks by the
            // entries' remaining quota and cooldowns.
            chip.on_click(cx.listener(move |this, _, _, cx| {
              if let Some(editor) = &mut this.editor {
                editor.strategy = strategy;
              }
              cx.notify();
            }))
          }))
          .child(gpui::div().text_size(px(12.)).text_color(cx.theme().muted_foreground).child("顺序 = 按列表；智能 = 按剩余额度挑账号；交替 = 不看额度挨个轮")),
      )
      .when(editor.strategy == RouteStrategy::Ordered, |this| {
        this.child(gpui::div().text_size(px(12.)).text_color(cx.theme().muted_foreground).child("列表顺序 = 派发顺序（用 ▲▼ 调整）"))
      })
      // The route itself: one flat list of model items, each with its key,
      // label input and a remove button.
      .child(gpui::div().text_size(px(12.)).text_color(cx.theme().muted_foreground).child("路由模型（点下方选择行的模型 chip 添加）"))
      .when(editor.items.is_empty(), |this| {
        this.child(gpui::div().text_size(px(12.)).text_color(cx.theme().muted_foreground).child("还没有模型"))
      })
      .children(editor.items.iter().enumerate().map(|(index, item)| {
        let key_label = data
          .keys
          .iter()
          .find(|key| key.id == item.key_id)
          .map(|key| key.label.clone())
          .unwrap_or_else(|| format!("账号 #{}", item.key_id));
        let field_label = |text: &str, cx: &App| {
          gpui::div().w(px(40.)).flex_shrink_0().text_size(px(12.)).text_color(cx.theme().muted_foreground).child(text.to_string())
        };
        gpui::div()
          .flex()
          .flex_col()
          .gap_1()
          .child(
            gpui::div()
              .flex()
              .items_center()
              .gap_1()
              // The item list IS the dispatch order — move buttons edit it.
              .child(
                Button::new(("route-item-up", index as u64))
                  .icon(Icon::new(IconName::ArrowUp).small())
                  .ghost()
                  .compact()
                  .disabled(index == 0)
                  .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor
                      && index > 0
                      && index < editor.items.len()
                    {
                      editor.items.swap(index, index - 1);
                    }
                    cx.notify();
                  })),
              )
              .child(
                Button::new(("route-item-down", index as u64))
                  .icon(Icon::new(IconName::ArrowDown).small())
                  .ghost()
                  .compact()
                  .disabled(index + 1 == item_count)
                  .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor
                      && index + 1 < editor.items.len()
                    {
                      editor.items.swap(index, index + 1);
                    }
                    cx.notify();
                  })),
              )
              .child(gpui::div().text_size(px(12.)).text_color(cx.theme().foreground).child(format!("{} · {}", key_label, item.model)))
              .child(gpui::div().flex_1())
              .child(Button::new(("route-item-remove", index as u64)).icon(Icon::new(IconName::Close).small()).ghost().compact().on_click(
                cx.listener(move |this, _, _, cx| {
                  if let Some(editor) = &mut this.editor {
                    editor.items.remove(index);
                  }
                  cx.notify();
                }),
              )),
          )
          .child(
            gpui::div()
              .flex()
              .items_center()
              .gap_2()
              .child(field_label("标签", cx))
              .child(gpui::div().w(px(200.)).flex_shrink_0().child(Input::new(&item.label)))
              .child(gpui::div().flex_1())
              .child(field_label("并发", cx))
              .child(gpui::div().w(px(72.)).flex_shrink_0().child(Input::new(&item.cap))),
          )
          .child(
            gpui::div()
              .flex()
              .flex_col()
              .gap_1p5()
              .child(
                gpui::div().flex().items_center().gap_2().child(field_label("规则", cx)).child(
                  gpui::div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child("从上往下首个命中生效；有规则时未命中 = 关闭；空 = 始终可用"),
                ),
              )
              .children(item.rules.iter().enumerate().map(|(rule_index, rule)| self.rule_row(index, rule_index, rule, cx)))
              .child(gpui::div().flex().child(field_label("", cx)).child(
                Button::new(("route-item-rule-add", index as u64)).label("＋ 添加规则").ghost().compact().on_click(cx.listener(
                  move |this, _, window, cx| {
                    if let Some(editor) = &mut this.editor
                      && let Some(item) = editor.items.get_mut(index)
                    {
                      item.rules.push(rule_draft(window, cx));
                    }
                    cx.notify();
                  },
                )),
              )),
          )
      }))
      .children(editor.rows.iter().enumerate().map(|(row_index, row)| self.picker_row(row_index, row, data, cx)))
      .child(
        Button::new("route-add-picker-row")
          .label("＋ 添加模型")
          .outline()
          .compact()
          .on_click(cx.listener(|this, _, _, cx| this.add_picker_row(cx))),
      )
      .when_some(editor.error.as_ref(), |this, error| {
        this.child(gpui::div().text_size(px(12.)).text_color(cx.theme().danger).child(error.clone()))
      })
      .child(
        gpui::div()
          .flex()
          .justify_end()
          .gap_2()
          .child(Button::new("route-editor-cancel").label("取消").outline().compact().on_click(cx.listener(|this, _, _, cx| {
            this.editor = None;
            cx.notify();
          })))
          .child(
            Button::new("route-editor-save")
              .label("保存")
              .primary()
              .compact()
              .disabled(self.manage_write_in_flight)
              .on_click(cx.listener(|this, _, _, cx| this.save_route(cx))),
          ),
      )
    }

    /// Render one availability-rule row of the route editor (item_index = the
    /// owning item, rule_index = the rule inside it). Top to bottom = priority.
    fn rule_row(
        &self,
        item_index: usize,
        rule_index: usize,
        rule: &RuleDraft,
        cx: &mut Context<Self>,
    ) -> Div {
        let effect_chip = |label: &'static str, active: bool, id: (&'static str, u64), cx: &App| {
            let chip = gpui::div()
                .id(id)
                .cursor_pointer()
                .border_1()
                .rounded_md()
                .px_2()
                .py_0p5()
                .text_size(px(11.))
                .child(label);
            if active {
                chip.border_color(cx.theme().accent)
                    .bg(cx.theme().accent)
                    .text_color(cx.theme().accent_foreground)
            } else {
                chip.border_color(cx.theme().border)
                    .text_color(cx.theme().muted_foreground)
            }
        };
        gpui::div()
            .flex()
            .items_center()
            .gap_2()
            .child(gpui::div().w(px(40.)).flex_shrink_0())
            // Effect toggle: 允许 / 关闭 (exactly one active).
            .child(
                effect_chip(
                    "允许",
                    rule.allow,
                    (
                        "route-item-rule-allow",
                        (item_index as u64) * 1024 + rule_index as u64,
                    ),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor
                        && let Some(item) = editor.items.get_mut(item_index)
                        && let Some(rule) = item.rules.get_mut(rule_index)
                    {
                        rule.allow = true;
                    }
                    cx.notify();
                })),
            )
            .child(
                effect_chip(
                    "关闭",
                    !rule.allow,
                    (
                        "route-item-rule-deny",
                        (item_index as u64) * 1024 + rule_index as u64,
                    ),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor
                        && let Some(item) = editor.items.get_mut(item_index)
                        && let Some(rule) = item.rules.get_mut(rule_index)
                    {
                        rule.allow = false;
                    }
                    cx.notify();
                })),
            )
            .child(
                gpui::div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .child(Input::new(&rule.window_start)),
            )
            .child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("~"),
            )
            .child(
                gpui::div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .child(Input::new(&rule.window_end)),
            )
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .children((1u8..=7).map(|day| {
                        let selected = rule.days.contains(&day);
                        let label = ["一", "二", "三", "四", "五", "六", "日"][(day - 1) as usize];
                        let chip = gpui::div()
                            .id((
                                "route-item-rule-day",
                                ((item_index as u64) * 1024 + rule_index as u64) * 8
                                    + u64::from(day),
                            ))
                            .cursor_pointer()
                            .border_1()
                            .rounded_md()
                            .px_1p5()
                            .py_0p5()
                            .text_size(px(11.))
                            .child(label);
                        let chip = if selected {
                            chip.border_color(cx.theme().accent)
                                .bg(cx.theme().accent)
                                .text_color(cx.theme().accent_foreground)
                        } else {
                            chip.border_color(cx.theme().border)
                                .text_color(cx.theme().muted_foreground)
                        };
                        chip.on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(editor) = &mut this.editor
                                && let Some(item) = editor.items.get_mut(item_index)
                                && let Some(rule) = item.rules.get_mut(rule_index)
                            {
                                if let Some(position) = rule.days.iter().position(|d| *d == day) {
                                    rule.days.remove(position);
                                } else {
                                    rule.days.push(day);
                                    rule.days.sort_unstable();
                                }
                            }
                            cx.notify();
                        }))
                    })),
            )
            .child(gpui::div().flex_1())
            .child(
                Button::new((
                    "route-item-rule-remove",
                    (item_index as u64) * 1024 + rule_index as u64,
                ))
                .icon(Icon::new(IconName::Close).small())
                .ghost()
                .compact()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(editor) = &mut this.editor
                        && let Some(item) = editor.items.get_mut(item_index)
                        && rule_index < item.rules.len()
                    {
                        item.rules.remove(rule_index);
                    }
                    cx.notify();
                })),
            )
    }

    /// One picker row: [账号选择按钮 ▾] (dropdown of accounts) then, once an
    /// account is chosen, its models as toggle chips (filtered by the account's
    /// whitelist). ✕ removes the row; items already added via it stay.
    fn picker_row(
        &self,
        row_index: usize,
        row: &PickerRow,
        data: &RoutesData,
        cx: &mut Context<Self>,
    ) -> Div {
        let account_label = match row.key_id {
            Some(key_id) => data
                .keys
                .iter()
                .find(|key| key.id == key_id)
                .map(|key| format!("{} · {}", key.provider_name, key.label))
                .unwrap_or_else(|| format!("账号 #{key_id}（已删除）")),
            None => "选择账号…".to_string(),
        };
        let chevron = if row.account_list_open { "▾" } else { "▸" };
        let account_list_open = row.account_list_open;
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
                            .id(ElementId::from(gpui::SharedString::from(format!(
                                "picker-row-account-{row_index}"
                            ))))
                            .cursor_pointer()
                            .border_1()
                            .rounded_md()
                            .border_color(cx.theme().border)
                            .px_2()
                            .py_0p5()
                            .text_size(px(12.))
                            .text_color(cx.theme().foreground)
                            .child(format!("{chevron} {account_label}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_account_list(row_index, !account_list_open, cx)
                            })),
                    )
                    .child(gpui::div().flex_1())
                    .child(
                        Button::new(("picker-row-remove", row_index as u64))
                            .icon(Icon::new(IconName::Close).small())
                            .ghost()
                            .compact()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_picker_row(row_index, cx)
                            })),
                    ),
            )
            .when(row.account_list_open, |this| {
                this.child(self.account_list(row_index, data, cx))
            })
            .when_some(row.key_id, |this, key_id| {
                let whitelist = data
                    .keys
                    .iter()
                    .find(|key| key.id == key_id)
                    .and_then(|key| key.allowed_models.as_deref());
                this.child(self.model_chips(key_id, &row.models, whitelist, cx))
            })
    }

    /// The inline account dropdown of one picker row: every enabled account,
    /// one clickable line each. Choosing one closes the list and fetches models.
    /// The id is needed for scrollability, so this returns a Stateful Div.
    fn account_list(
        &self,
        row_index: usize,
        data: &RoutesData,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let mut list = gpui::div()
            .flex()
            .flex_col()
            .ml_4()
            .border_1()
            .rounded_md()
            .border_color(cx.theme().border)
            .max_h(px(240.))
            .id(ElementId::from(gpui::SharedString::from(format!(
                "picker-account-list-{row_index}"
            ))))
            .overflow_y_scroll();
        if data.keys.is_empty() {
            list = list.child(
                gpui::div()
                    .p_2()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("没有账号——先在账号页添加"),
            );
        }
        for key in &data.keys {
            let key_id = key.id;
            list = list.child(
                gpui::div()
                    .id(ElementId::from(gpui::SharedString::from(format!(
                        "picker-account-{row_index}-{key_id}"
                    ))))
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(cx.theme().foreground)
                    .hover(|this| this.bg(cx.theme().accent))
                    .child(format!("{} · {}", key.provider_name, key.label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_row_account(row_index, key_id, cx)
                    })),
            );
        }
        list
    }

    /// The fetched models of one row's account as toggle chips: click adds the
    /// (key, model) item to the route's flat list, click again removes it.
    /// Attributes the list carried show as suffix badges on the chip. Models
    /// outside the account's whitelist (`allowed_models`) are not shown at all —
    /// a route can only reference models the account actually serves.
    fn model_chips(
        &self,
        key_id: i64,
        models: &Option<LoadState<Vec<ProbeModel>>>,
        whitelist: Option<&[String]>,
        cx: &mut Context<Self>,
    ) -> Div {
        let row = gpui::div().flex().flex_wrap().items_center().gap_1().pl_6();
        match models {
            None | Some(LoadState::Loading) => row.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("加载模型…"),
            ),
            Some(LoadState::Failed(error)) => row
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(format!("模型列表获取失败：{error}")),
                )
                .child(
                    Button::new(("models-retry", key_id as u64))
                        .label("重试")
                        .ghost()
                        .compact()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.retry_key_models(key_id, cx)),
                        ),
                ),
            Some(LoadState::Loaded(models)) => {
                // Whitelist gate: None/empty = every probed model is selectable.
                let visible: Vec<&ProbeModel> = models
                    .iter()
                    .filter(|model| whitelist_allows(whitelist, &model.id))
                    .collect();
                if visible.is_empty() {
                    return row.child(
                        gpui::div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("该账号无可用模型（白名单已限制）"),
                    );
                }
                row.children(visible.into_iter().map(|model| {
                    let model_id = model.id.clone();
                    let in_route = self.editor.as_ref().is_some_and(|editor| {
                        editor
                            .items
                            .iter()
                            .any(|item| item.key_id == key_id && item.model == model.id)
                    });
                    // Attributes the list carried, as suffix badges on the chip:
                    // id · 上下文 · 输出上限 · 模态 · 推理级别.
                    let mut chip_text = model.id.clone();
                    if let Some(context) = model.context {
                        chip_text.push_str(&format!(
                            " · {}",
                            crate::fmt::humanize_count(context as i64)
                        ));
                    }
                    if let Some(output) = model.max_output {
                        chip_text.push_str(&format!(
                            " · 出 {}",
                            crate::fmt::humanize_count(output as i64)
                        ));
                    }
                    for modality in &model.input_modalities {
                        let badge = match modality.as_str() {
                            "image" => "图",
                            "audio" => "音",
                            "video" => "视",
                            other => other,
                        };
                        chip_text.push_str(&format!(" · {badge}"));
                    }
                    if !model.effort_levels.is_empty() {
                        chip_text.push_str(" · 推理");
                    }
                    let chip = gpui::div()
                        .id(ElementId::from(gpui::SharedString::from(format!(
                            "model-chip-{key_id}-{model_id}"
                        ))))
                        .cursor_pointer()
                        .border_1()
                        .rounded_md()
                        .px_2()
                        .py_0p5()
                        .text_size(px(12.))
                        .child(chip_text);
                    let chip = if in_route {
                        chip.border_color(cx.theme().accent)
                            .bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    } else {
                        chip.border_color(cx.theme().border)
                            .text_color(cx.theme().foreground)
                            .hover(|this| this.bg(cx.theme().accent))
                    };
                    chip.on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_model(key_id, model_id.clone(), window, cx)
                    }))
                }))
            }
        }
    }
}

impl crate::pages::ActionPage for RoutesPage {
    fn action_error_slot(&mut self) -> &mut Option<String> {
        &mut self.action_error
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        RoutesPage::reload(self, cx);
    }
}

impl Render for RoutesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No header: the tab bar names the view and the actions live in the tab
        // row, so the content starts immediately.
        let frame = page_frame("", "", cx);
        match &self.state {
            LoadState::Loading => frame.child(loading_card(cx)),
            LoadState::Failed(error) => frame.child(error_card(error, cx)),
            LoadState::Loaded(data) => {
                let frame = frame
                    // The stage renders even with no requests yet: an empty recent
                    // list shows the skeleton (client/cardinal nodes, no wires) so the
                    // layout is inspectable before any traffic has flowed. Its right
                    // column IS the route list — every route as a section.
                    .when_some(self.stage_card(data, cx), |this, stage| this.child(stage));
                frame
                    .when_some(self.action_error.as_ref(), |this, error| {
                        this.child(action_error_card(error, cx))
                    })
                    .when_some(self.editor.as_ref(), |this, editor| {
                        this.child(self.route_editor_card(editor, data, cx))
                    })
                    .when(data.routes.is_empty(), |this| {
                        this.child(empty_card(
                            "尚无路由",
                            "路由把一组账号暴露为一个模型 ID；先在账号页添加账号。",
                            cx,
                        ))
                    })
            }
        }
    }
}

/// Does the account's model whitelist permit this model? None/empty =
/// unrestricted (every probed model is selectable). Mirrors the relay's
/// `serves_model` so the picker never offers a model dispatch would skip.
fn whitelist_allows(whitelist: Option<&[String]>, model_id: &str) -> bool {
    whitelist.is_none_or(|list| list.is_empty() || list.iter().any(|id| id == model_id))
}

// ---------- live stage ----------
//
// Fixed geometry stands in for DOM measurement: node/row heights
// are constants, so the wire endpoints are computed arithmetically and the
// overlay canvas needs no prepaint. Column widths and gaps match the CSS
// (.rt-stage: 124px 124px 1fr, column-gap 56px, padding 22px 2px).
