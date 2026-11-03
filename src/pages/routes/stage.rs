//! The routes page's live stage: caller nodes → the cardinal hub → every
//! route's entries stacked as sections, with dashed wires that flow while a
//! call is in flight. Pure visualization — all reads, no writes; route
//! management (CRUD, editor, manage mode) lives in the parent module.

use std::rc::Rc;

use gpui::{
    App, AppContext as _, Bounds, Context, Div, ElementId, Hsla, InteractiveElement, ParentElement,
    PathBuilder, StatefulInteractiveElement, Styled, Window, canvas, point,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable};

use gateway::store::{ActiveWindow, ModelSuccessRow, RestRow, RouteRow, RouteStrategy};

use super::{RouteAccount, RoutesData, RoutesPage};
use crate::activity::{client_hue, named_client};
use crate::pages::{LoadState, card};

const CLIENT_LINGER: std::time::Duration = std::time::Duration::from_secs(300);

/// Human-readable window text: 工作日/周…/HH:MM~HH:MM (whole-day with days
/// shows only the days).
fn describe_window(window: &ActiveWindow) -> String {
    let days = window
        .days
        .as_ref()
        .filter(|days| !days.is_empty())
        .map(|days| {
            // 1..=5 → "工作日" (the common case), otherwise "周一周四"-style.
            if days == &[1, 2, 3, 4, 5] {
                "工作日".to_string()
            } else {
                let names = ["一", "二", "三", "四", "五", "六", "日"];
                format!(
                    "周{}",
                    days.iter()
                        .map(|day| names
                            .get(usize::from(day.saturating_sub(1)))
                            .copied()
                            .unwrap_or("?"))
                        .collect::<String>()
                )
            }
        });
    match days {
        Some(days) if window.start == window.end => days,
        Some(days) => format!("{days} {}~{}", window.start, window.end),
        None => format!("{}~{}", window.start, window.end),
    }
}
/// At most this many client nodes on stage.
const MAX_CLIENTS: usize = 4;

/// One client node of the stage, derived from a caller key's label.
pub(super) struct StageClient {
    /// Display name (label minus the `cardinal-` prefix; 本机 for loopback).
    name: String,
    /// The last client-sent model id (node subtitle).
    sub: String,
    /// Last request activity — the linger clock. Live clients never expire.
    last_seen: std::time::Instant,
    /// A call of this client is in flight (its wire flows).
    live: bool,
}

impl RoutesPage {
    /// Seed the client registry from the ledger (a GUI start or refresh
    /// restores who was connected within the linger window). Entries that
    /// pre-date this pass win untouched — a live client's clock is fresher
    /// than any ledger row — while a client seen on multiple rows keeps its
    /// first (chronological) node position with its NEWEST row's sub/last_seen.
    pub(super) fn seed_clients(&mut self) {
        let LoadState::Loaded(data) = &self.state else {
            return;
        };
        let now = std::time::Instant::now();
        let utc_now = chrono::Utc::now();
        let seeded_from = self.clients.len();
        // recent is newest-first; iterate in reverse for chronological node order.
        for request in data.recent.iter().rev() {
            let age = (utc_now - request.ts)
                .to_std()
                .unwrap_or(std::time::Duration::ZERO);
            if age >= CLIENT_LINGER {
                continue;
            }
            let name = named_client(request.agent.as_deref(), request.caller_key.as_deref());
            if let Some(index) = self.clients.iter().position(|client| client.name == name) {
                // Only entries added by this pass update: pre-existing (live) ones win.
                if index >= seeded_from {
                    let client = &mut self.clients[index];
                    client.sub = request
                        .model
                        .clone()
                        .or_else(|| request.alias.clone())
                        .unwrap_or_default();
                    client.last_seen = now.checked_sub(age).unwrap_or(now);
                }
                continue;
            }
            self.clients.push(StageClient {
                name,
                sub: request
                    .model
                    .clone()
                    .or_else(|| request.alias.clone())
                    .unwrap_or_default(),
                last_seen: now.checked_sub(age).unwrap_or(now),
                live: false,
            });
        }
        self.prune_clients();
    }

    /// Fold the live-call snapshot into the client registry: live callers
    /// upsert (name derived from the label's `cardinal-` suffix), clients whose
    /// last call just settled start their linger clock, expired ones drop out.
    /// Any caller appears — routed or direct — every client wires to the hub.
    pub(super) fn sync_clients(&mut self, live: Vec<(String, String)>, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        // One entry per client: first-appearance order, latest model as sub.
        let mut deduped: Vec<(String, String)> = Vec::new();
        for (name, sub) in live {
            match deduped.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) => entry.1 = sub,
                None => deduped.push((name, sub)),
            }
        }
        let live = deduped;
        for (name, sub) in &live {
            match self.clients.iter_mut().find(|client| &client.name == name) {
                Some(client) => {
                    client.live = true;
                    client.last_seen = now;
                    client.sub = sub.clone();
                }
                None => self.clients.push(StageClient {
                    name: name.clone(),
                    sub: sub.clone(),
                    last_seen: now,
                    live: true,
                }),
            }
        }
        for client in &mut self.clients {
            if client.live && !live.iter().any(|(name, _)| name == &client.name) {
                client.live = false;
                client.last_seen = now;
            }
        }
        self.prune_clients();
        self.arm_prune(cx);
    }

    /// Drop expired clients (resting, last_seen older than CLIENT_LINGER) and
    /// enforce MAX_CLIENTS by evicting the oldest resting nodes; live clients
    /// are never evicted. Nodes keep their positions while they stay —
    /// new ones append.
    fn prune_clients(&mut self) {
        let now = std::time::Instant::now();
        self.clients.retain(|client| {
            client.live || now.saturating_duration_since(client.last_seen) < CLIENT_LINGER
        });
        while self.clients.len() > MAX_CLIENTS {
            let Some((index, _)) = self
                .clients
                .iter()
                .enumerate()
                .filter(|(_, client)| !client.live)
                .min_by_key(|(_, client)| client.last_seen)
            else {
                break;
            };
            self.clients.remove(index);
        }
    }

    /// Arm a one-shot timer for the nearest resting client's expiry (no
    /// polling); re-arms itself while resting clients remain. A timer armed
    /// for the same deadline is kept as-is; `clients_gen` invalidates timers
    /// armed against an older registry.
    pub(super) fn arm_prune(&mut self, cx: &mut Context<Self>) {
        let Some(soonest) = self
            .clients
            .iter()
            .filter(|client| !client.live)
            .map(|client| client.last_seen + CLIENT_LINGER)
            .min()
        else {
            self.prune_armed_until = None;
            return;
        };
        if self.prune_armed_until == Some(soonest) {
            return;
        }
        self.prune_armed_until = Some(soonest);
        self.clients_gen += 1;
        let generation = self.clients_gen;
        let wait = soonest
            .saturating_duration_since(std::time::Instant::now())
            .max(std::time::Duration::from_millis(200));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |page, cx| {
                if page.clients_gen != generation {
                    return;
                }
                page.prune_clients();
                cx.notify();
                // The armed deadline's timer has fired by definition — clear the
                // record before re-arming, or an early-firing timer (sub-ms wheel
                // granularity) would match the keep-check and leave nothing armed.
                page.prune_armed_until = None;
                page.arm_prune(cx);
            })
            .ok();
        })
        .detach();
    }

    /// One 66ms beat of the wire-flow animation; re-arms itself (from the
    /// observer, on the next event) while the feed is non-idle. Advances the
    /// dash phase at the wire-flow speed (8px per 0.7s).
    pub(super) fn arm_anim(&mut self, cx: &mut Context<Self>) {
        if self.anim_armed {
            return;
        }
        self.anim_armed = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(66))
                .await;
            this.update(cx, |page, cx| {
                page.anim_armed = false;
                page.phase = (page.phase + 0.75) % 8.0;
                cx.notify();
                // Any in-flight call flows its caller's wire (routed ones also
                // light an entry row); idle feed = no repaint for nothing.
                if !page.feed.read(cx).is_idle() {
                    page.arm_anim(cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

const STAGE_PAD_Y: f32 = 22.;
/// Caller column width (grid column 1).
const SRC_W: f32 = 124.;
/// Hub column width (grid column 2).
const HUB_W: f32 = 124.;
/// Gap between the three columns (`column-gap: 56px`).
const COL_GAP: f32 = 56.;
/// Fixed node heights (source and hub share the card geometry).
const NODE_H: f32 = 64.;
/// Gap between stacked source nodes (.rt-srcs `gap: 10px`).
const SRC_GAP: f32 = 10.;
/// Fixed account row height (.rt-accts li, single line, no bar).
const ACCT_H: f32 = 40.;
/// Gap between account rows (.rt-accts `gap: 7px`).
const ACCT_GAP: f32 = 7.;
/// Section header height (alias line) in the stacked routes column.
const SEC_HEAD_H: f32 = 28.;
/// Gap between route sections.
const SEC_GAP: f32 = 14.;
/// Gap between a section header and its first entry row.
const SEC_INNER_GAP: f32 = 6.;

/// One source node of the stage: a connected client (or the placeholder
/// skeleton before any traffic ever).
struct StageSource {
    /// Client name (key label minus `cardinal-`; 本机 for loopback).
    name: String,
    /// The last client-sent model id (node subtitle).
    sub: String,
    /// A call of this client is in flight right now (wire flows).
    live: bool,
    /// The client's stable hue; None = placeholder node,
    /// which draws no wire.
    hue: Option<Hsla>,
}

/// One row of the stage: a route entry, i.e. one account on one model.
struct StageAccount {
    /// The account's display name (label · provider), or the deleted stub when
    /// the entry dangles.
    head: String,
    /// Display name of the served model (entry label or model id), as a chip.
    model: String,
    /// Entry constraints as a dim suffix (并发 N · HH:MM~HH:MM); "" when none.
    extras: String,
    /// Phase caption while serving (连接中/传输中/接收中); None when idle.
    caption: Option<&'static str>,
    /// This entry is serving a live call right now (row on, wire flows).
    live: bool,
    /// Cooldown recovery caption (冷却至 10-19 08:00); Some = the entry rests —
    /// the row renders dimmed with a warning dot.
    cool_until: Option<String>,
    /// This entry cannot dispatch: its account was deleted, is disabled or
    /// archived, its provider is disabled, or its whitelist omits the entry's
    /// model. The row renders dimmed with a muted filled dot and the reason.
    stopped: Option<Stopped>,
    /// Latest successful request time for this account on this model — shown in
    /// the serving-caption slot once the entry goes idle.
    last_success: Option<chrono::DateTime<chrono::Utc>>,
}

/// One in-flight call of this route, as the entry aggregation reads it. The
/// live feed's attempt type carries display strings and private fields; the
/// aggregation only needs identity plus the phase caption.
struct LiveCall<'call> {
    /// The account serving the call (None = not yet routed to one).
    key_id: Option<i64>,
    /// The upstream model, in its clipped wire form.
    model: &'call str,
    /// Phase caption (连接中/传输中/接收中).
    phase: &'static str,
    /// The call has seen its first byte, so it outranks one still connecting.
    /// Kept apart from the caption: the caption is copy, this is the ordering.
    progressed: bool,
}

/// Why one route entry cannot dispatch anything right now.
///
/// `pub(super)` so the routes page renders the same reasons on its stage rows,
/// its editor item rows, its picker lines and its model-chip guard; the stage is
/// where they are computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stopped {
    /// The entry points at an account the document no longer carries.
    Deleted,
    /// The account is disabled on the accounts page — a reversible state.
    AccountDisabled,
    /// The account is archived: kept for history. Dispatch skips it while the
    /// archive stands, whatever its own enabled flag says; 移出归档 on the
    /// accounts page is what restores it.
    Archived,
    /// The account is fine but its provider is disabled, so dispatch builds no
    /// candidate from it.
    ProviderDisabled,
    /// The account is dispatchable but does not serve this entry's model: its
    /// whitelist omits it, so dispatch would refuse the only candidate.
    NoModel,
}

impl Stopped {
    /// The marker text for this reason — shared by the stage rows, the picker
    /// lines, the editor's item rows and the chip guard, so one state cannot be
    /// described two ways.
    pub(super) fn label(self) -> &'static str {
        match self {
            Stopped::Deleted => "账号已删除",
            Stopped::AccountDisabled => "账号已停用",
            Stopped::Archived => "账号已归档",
            Stopped::ProviderDisabled => "供应商已停用",
            Stopped::NoModel => "该账号不支持此模型",
        }
    }
}

/// Why this account cannot dispatch. The three conditions are conjunctive on
/// the wire (dispatch asks for an enabled provider, an enabled account and no
/// archive), so the order here only decides which one to show: an archived
/// account cannot be restored by flipping its own flag, so its own state is the
/// useful answer before its provider's.
///
/// The single owner of the account-level half of the rule: [`entry_stopped`]
/// adds the whitelist, and every surface renders [`Stopped::label`] from these
/// two instead of re-deriving the conditions. The chip guard asks only about
/// archiving on purpose — a disabled account stays addable, since flipping the
/// flag back restores it.
pub(super) fn stopped_of(account: &RouteAccount) -> Option<Stopped> {
    if account.archived {
        Some(Stopped::Archived)
    } else if !account.enabled {
        Some(Stopped::AccountDisabled)
    } else if !account.provider_enabled {
        Some(Stopped::ProviderDisabled)
    } else {
        None
    }
}

/// Why this route entry as a whole cannot dispatch: the account's own state
/// first (deleted / archived / disabled / provider disabled), then its model
/// whitelist. The stage row and the editor's item row both read this one
/// function, so the two surfaces cannot drift apart.
pub(super) fn entry_stopped(account: Option<&RouteAccount>, model: &str) -> Option<Stopped> {
    match account {
        None => Some(Stopped::Deleted),
        Some(account) => {
            stopped_of(account).or_else(|| (!account.serves(model)).then_some(Stopped::NoModel))
        }
    }
}

/// One entry's stage state: the account it names, on the entry's model.
///
/// An entry IS one account on one model, so every fact here is that single
/// account's — serving, the cooldown and the last success are (key, model)
/// lookups, never a spread over several accounts. Pure: every input is a read
/// the caller already has.
#[derive(Debug, Default, PartialEq)]
struct EntryState {
    /// Phase caption of the call serving this entry (None = idle).
    serving: Option<&'static str>,
    /// The entry cannot dispatch (deleted account, disabled account or
    /// provider, or a model the account does not serve).
    stopped: Option<Stopped>,
    /// The moment dispatch may use this entry again: set only while the account
    /// is cooling on this model and the entry is not stopped for another
    /// reason.
    cool_until: Option<chrono::DateTime<chrono::Utc>>,
    /// Newest success of this account on this model.
    last_success: Option<chrono::DateTime<chrono::Utc>>,
}

/// Aggregate one entry's stage state over the single account it names.
///
/// * serving — the most advanced in-flight call of this route on this account
///   and this model (compared in the model's clipped wire form), so the caption
///   shows the phase that actually moved furthest;
/// * cooling — this account cooling on this model at `now` (a rest with a NULL
///   model cools the whole account, so it covers every model; a rest whose
///   `rest_until` is not after `now` is over);
/// * stopped — the account is gone, cannot dispatch, or does not serve the
///   entry's model;
/// * last success — the newest success of this account on this model.
fn entry_state(
    account: Option<&RouteAccount>,
    model: &str,
    calls: &[LiveCall<'_>],
    rests: &[RestRow],
    success: &[ModelSuccessRow],
    now: chrono::DateTime<chrono::Utc>,
) -> EntryState {
    let stopped = entry_stopped(account, model);
    let Some(account) = account else {
        // The entry dangles: no account to aggregate over.
        return EntryState {
            stopped,
            ..EntryState::default()
        };
    };
    let key_id = account.key_id;
    let mut state = EntryState {
        stopped,
        ..EntryState::default()
    };
    if state.stopped.is_none() {
        // Only a cooldown still in force at `now`: the gateway drops expired
        // rows only when it builds the document, and nothing refreshes the page
        // when a cooldown ends, so an expired row must not read as resting.
        state.cool_until = rests
            .iter()
            .filter(|rest| {
                rest.key_id == key_id
                    && rest.rest_until > now
                    && rest
                        .model
                        .as_deref()
                        .is_none_or(|rest_model| rest_model == model)
            })
            .map(|rest| rest.rest_until)
            .max();
    }
    // The entry's calls are this route's, on this account and this model.
    // Ties keep feed order, so the choice is stable across renders: only a call
    // that has seen its first byte displaces one that has not.
    let clipped = crate::activity::clipped(model);
    let mut serving: Option<&LiveCall<'_>> = None;
    for call in calls {
        if call.model != clipped || call.key_id != Some(key_id) {
            continue;
        }
        if serving.is_none_or(|best| call.progressed && !best.progressed) {
            serving = Some(call);
        }
    }
    state.serving = serving.map(|serving| serving.phase);
    state.last_success = success
        .iter()
        .filter(|row| row.served_model == model && row.key_id == key_id)
        .map(|row| row.last_ok)
        .max();
    state
}

/// The 冷却至 marker of a cooling entry: the time alone when `until` falls on
/// the same day as `now` in the display offset `tz`, otherwise with its date.
/// `now` is the render's single clock read, the same one the cooldown-expiry
/// check in [`entry_state`] used, so the two always agree.
#[must_use]
fn cool_until_label(
    until: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    tz: chrono::FixedOffset,
) -> String {
    let local = until.with_timezone(&tz);
    if local.date_naive() == now.with_timezone(&tz).date_naive() {
        format!("冷却至 {}", local.format("%H:%M"))
    } else {
        format!("冷却至 {}", local.format("%m-%d %H:%M"))
    }
}

/// Drag payload of one stage entry row: source route alias + index plus a
/// ghost label. Dropping onto another route's section is ignored —
/// reordering stays within one route (single atomic put_route).
#[derive(Clone)]
pub(super) struct StageEntryDrag {
    pub(super) alias: String,
    pub(super) from: usize,
    pub(super) label: String,
}

/// One route section of the stage: alias header over its entries.
struct StageSection {
    /// Route alias (also the section's identity for manage actions).
    alias: String,
    /// Declared context window (header badge; None = undeclared).
    context_window: Option<i64>,
    /// Entries in dispatch order.
    accounts: Vec<StageAccount>,
}

/// The resolved stage: sources on the left, every route as a section on the
/// right, and whether wires connect them at all.
pub(super) struct StageModel {
    sources: Vec<StageSource>,
    sections: Vec<StageSection>,
    /// False before any traffic ever: the skeleton shows nodes without wires.
    wired: bool,
}

/// One bezier wire between two columns, in stage-local coordinates.
struct Wire {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    /// Live wires draw as an animated dash flow
    /// (dasharray 3 5, 0.7s linear infinite); resting wires
    /// are solid strokes.
    live: bool,
    /// Stroke colour: the client's hue on caller wires (dimmed when resting),
    /// accent/border on hub wires.
    color: Hsla,
}

impl RoutesPage {
    /// Resolve what the stage shows: every route as a section (its entries in
    /// dispatch order, live ones marked serving), and on the left the callers
    /// with a routed call in flight (else the most recent caller resting, else
    /// a placeholder). None when there are no routes (the empty card renders).
    pub(super) fn stage_model(&self, data: &RoutesData, cx: &App) -> Option<StageModel> {
        if data.routes.is_empty() {
            return None;
        }
        // One clock read for the whole render: cooldown expiry and the 冷却至
        // date comparison must agree with each other. The display offset is
        // read once alongside it for the same reason.
        let now = chrono::Utc::now();
        let tz = crate::ui_tz::display_tz();
        let feed = self.feed.read(cx);
        // Every routed in-flight attempt. Wire routes are clipped to 64 bytes;
        // compare aliases in the clipped form so a long alias still matches. Two
        // aliases sharing a 64-byte prefix would cross-light each other's
        // sections — a wire-protocol limitation, not resolvable on this page.
        let live: Vec<&crate::activity::LiveAttempt> = feed
            .live()
            .filter(|attempt| attempt.route.is_some())
            .collect();
        // Sources: the client registry (live callers + lingering ones), each
        // with its stable hue; a placeholder node keeps the skeleton inspectable
        // before any traffic ever, and draws no wire.
        let mut sources: Vec<StageSource> = self
            .clients
            .iter()
            .map(|client| StageSource {
                name: client.name.clone(),
                sub: client.sub.clone(),
                live: client.live,
                hue: Some(client_hue(&client.name)),
            })
            .collect();
        let has_clients = !sources.is_empty();
        if sources.is_empty() {
            sources.push(StageSource {
                name: "客户端".to_string(),
                sub: "尚无请求".to_string(),
                live: false,
                hue: None,
            });
        }
        // Wires exist only with at least one real client on the left AND routed
        // traffic to show on the right (in flight, or in the recent ledger).
        let wired = has_clients
            && (!live.is_empty() || data.recent.iter().any(|request| request.alias.is_some()));

        let sections = data
            .routes
            .iter()
            .map(|route| {
                let alias = crate::activity::clipped(&route.alias);
                // This route's in-flight calls, as the per-entry aggregation
                // reads them (identity only: key id, clipped model, phase).
                let calls: Vec<LiveCall<'_>> = live
                    .iter()
                    .filter(|attempt| attempt.route.as_deref() == Some(alias))
                    .map(|attempt| LiveCall {
                        key_id: attempt.key_id,
                        model: &attempt.served_model,
                        phase: attempt.phase(),
                        progressed: attempt.streaming,
                    })
                    .collect();
                let accounts = route
                    .entries
                    .iter()
                    .map(|entry| {
                        // An entry names one account on one model: the row is
                        // that account's own state on that model.
                        let account = data.account(entry.key_id);
                        let head = super::account_head(account, entry.key_id);
                        let state = entry_state(
                            account,
                            &entry.model,
                            &calls,
                            &data.rests,
                            &data.success,
                            now,
                        );
                        let extras = [entry.max_concurrency.map(|cap| format!("并发 {cap}")), {
                            let rules = entry.rules();
                            if rules.is_empty() {
                                None
                            } else {
                                Some(
                                    rules
                                        .iter()
                                        .map(|rule| {
                                            let effect = match rule.effect {
                                                gateway::store::RuleEffect::Allow => "允许",
                                                gateway::store::RuleEffect::Deny => "关闭",
                                            };
                                            format!("{effect} {}", describe_window(&rule.window))
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" · "),
                                )
                            }
                        }]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                        let cool_until = state
                            .cool_until
                            .map(|until| cool_until_label(until, now, tz));
                        StageAccount {
                            head,
                            model: entry.label.clone().unwrap_or_else(|| entry.model.clone()),
                            extras,
                            caption: state.serving,
                            live: state.serving.is_some(),
                            cool_until,
                            stopped: state.stopped,
                            last_success: state.last_success,
                        }
                    })
                    .collect();
                StageSection {
                    alias: route.alias.clone(),
                    context_window: route.context_window,
                    accounts,
                }
            })
            .collect();
        Some(StageModel {
            sources,
            sections,
            wired,
        })
    }

    /// The live stage card: caller nodes — hub — every
    /// route's entries as stacked sections, wired by beziers that flow
    /// (animated dashes) while a call is in flight over them. In manage mode
    /// each section header carries 编辑/删除 and each entry row its ▲▼/✕.
    pub(super) fn stage_card(&self, data: &RoutesData, cx: &mut Context<Self>) -> Option<Div> {
        let model = self.stage_model(data, cx)?;
        let theme = cx.theme();
        let manage = self.manage;

        // Column heights (fixed geometry) → wire endpoints, no DOM measurement.
        // An entry-less route still renders one placeholder row.
        let section_h = |section: &StageSection| {
            let rows = section.accounts.len().max(1) as f32;
            SEC_HEAD_H + SEC_INNER_GAP + rows * ACCT_H + (rows - 1.) * ACCT_GAP
        };
        let n_sources = model.sources.len();
        // sources is never empty (the placeholder node guarantees it), so
        // sources_h >= NODE_H always holds.
        let sources_h = n_sources as f32 * NODE_H + (n_sources.saturating_sub(1)) as f32 * SRC_GAP;
        let accounts_h = model.sections.iter().map(&section_h).sum::<f32>()
            + (model.sections.len().saturating_sub(1)) as f32 * SEC_GAP;
        let content_h = sources_h.max(accounts_h);
        let stage_h = content_h + 2. * STAGE_PAD_Y;
        let hub_y = STAGE_PAD_Y + content_h / 2.;

        // Wires: caller right edge → hub left edge (one per client, in its hue —
        // flowing while it has a call in flight, dimmed solid while it lingers),
        // hub right edge → every entry row's left edge across all sections. The
        // inner row carries the stage's 2px horizontal padding, so the x offsets
        // land exactly on the row edges: 126 → 182, 306 → 362.
        let mut wires = Vec::new();
        if model.wired {
            let x_src = 2. + SRC_W;
            let x_hub = x_src + COL_GAP;
            for (index, source) in model.sources.iter().enumerate() {
                let Some(hue) = source.hue else { continue };
                let y = STAGE_PAD_Y
                    + (content_h - sources_h) / 2.
                    + index as f32 * (NODE_H + SRC_GAP)
                    + NODE_H / 2.;
                let mut color = hue;
                if !source.live {
                    color.a = 0.35;
                }
                wires.push(Wire {
                    x1: x_src,
                    y1: y,
                    x2: x_hub,
                    y2: hub_y,
                    live: source.live,
                    color,
                });
            }
            let x_hub_r = x_hub + HUB_W;
            let x_acct = x_hub_r + COL_GAP;
            let mut section_y = STAGE_PAD_Y + (content_h - accounts_h) / 2.;
            for section in &model.sections {
                let mut row_y = section_y + SEC_HEAD_H + SEC_INNER_GAP;
                for account in &section.accounts {
                    wires.push(Wire {
                        x1: x_hub_r,
                        y1: hub_y,
                        x2: x_acct,
                        y2: row_y + ACCT_H / 2.,
                        live: account.live,
                        color: if account.live {
                            theme.accent
                        } else {
                            theme.border
                        },
                    });
                    row_y += ACCT_H + ACCT_GAP;
                }
                section_y += section_h(section) + SEC_GAP;
            }
        }
        let wires = Rc::new(wires);
        let phase = self.phase;
        let overlay = canvas(
            move |_bounds, _window, _cx| wires.clone(),
            move |bounds, wires, window, _cx| paint_wires(bounds, &wires, window, phase),
        )
        .absolute()
        .inset_0()
        .size_full();

        // The serving-dot pulse shares the flow phase.
        let pulse = (self.phase / 8. * std::f32::consts::TAU).sin() * 0.5 + 0.5;

        let sources_col = gpui::div()
            .w(px(SRC_W))
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(SRC_GAP))
            .children(model.sources.iter().map(|source| {
                // The client's hue marks its node; it pulses while
                // a call is in flight, the placeholder stays muted.
                let mut dot_color = source.hue.unwrap_or(theme.muted_foreground);
                if source.live {
                    dot_color.a = 0.45 + 0.55 * pulse;
                }
                gpui::div()
                    .w_full()
                    .h(px(NODE_H))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(2.))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(12.))
                    .overflow_hidden()
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(gpui::div().w(px(7.)).h(px(7.)).rounded_full().bg(dot_color))
                            .child(
                                gpui::div()
                                    .text_size(px(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(source.name.clone()),
                            ),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(source.sub.clone()),
                    )
            }));

        let hub_col = gpui::div()
            .w(px(HUB_W))
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .child(
                gpui::div()
                    .w_full()
                    .h(px(NODE_H))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(2.))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.foreground)
                    .rounded(px(12.))
                    .overflow_hidden()
                    .child(
                        gpui::div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("cardinal"),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(self.hub_addr.clone()),
                    ),
            );

        let mut accounts_col = gpui::div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(SEC_GAP));
        for (route, section) in data.routes.iter().zip(model.sections.iter()) {
            accounts_col =
                accounts_col.child(self.stage_section(route, section, manage, pulse, cx));
        }

        Some(
            card(cx).child(
                gpui::div()
                    .relative()
                    .h(px(stage_h))
                    // Capture order is outer→inner: clear first, then the row
                    // under the cursor sets its section's drop site again.
                    .on_drag_move(cx.listener(
                        |this, _: &gpui::DragMoveEvent<StageEntryDrag>, _, cx| {
                            if this.stage_drop.take().is_some() {
                                cx.notify();
                            }
                        },
                    ))
                    .child(overlay)
                    .child(
                        gpui::div()
                            .h_full()
                            .py(px(STAGE_PAD_Y))
                            .px(px(2.))
                            .flex()
                            .child(sources_col)
                            .child(gpui::div().w(px(COL_GAP)).flex_shrink_0())
                            .child(hub_col)
                            .child(gpui::div().w(px(COL_GAP)).flex_shrink_0())
                            .child(accounts_col),
                    ),
            ),
        )
    }

    /// One route section of the stage: the alias header (carrying the route's
    /// 编辑/删除 actions in manage mode) over its entry rows in dispatch order.
    fn stage_section(
        &self,
        route: &RouteRow,
        section: &StageSection,
        manage: bool,
        pulse: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        // Copied out of `theme` (Hsla is Copy): the drag insertion line below
        // reads it after the `&mut cx` stage_entry loop, which must not
        // overlap `theme`'s borrow.
        let accent = theme.accent;
        let alias = route.alias.clone();
        let edit_route = route.clone();
        let header = gpui::div()
            .h(px(SEC_HEAD_H))
            .flex()
            .items_center()
            .gap_2()
            .child(
                gpui::div()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(section.alias.clone()),
            )
            .child(Tag::secondary().child(format!("{} 项", section.accounts.len())))
            .when_some(section.context_window, |this, window| {
                this.child(
                    Tag::secondary()
                        .child(format!("上下文 {}", crate::fmt::humanize_count(window))),
                )
            })
            // Non-ordered strategy marker: the header says which at a glance.
            .when(route.strategy != RouteStrategy::Ordered, |this| {
                let label = match route.strategy {
                    RouteStrategy::Smart => "智能",
                    RouteStrategy::Rotate => "交替",
                    RouteStrategy::Ordered => unreachable!(),
                };
                this.child(Tag::secondary().child(label))
            })
            .child(gpui::div().flex_1())
            // Route note (备注): dim, right-aligned, truncated — display only.
            .when_some(route.note.as_deref(), |this, note| {
                this.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .max_w(px(360.))
                        .text_ellipsis()
                        .child(note.to_string()),
                )
            })
            .when(manage, |this| {
                let busy = self.manage_write_in_flight;
                this.child(
                    Button::new((ElementId::from("route-edit"), alias.clone()))
                        .icon(Icon::new(IconName::Pencil).small())
                        .label("编辑")
                        .outline()
                        .compact()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_route_editor(Some(edit_route.clone()), window, cx)
                        })),
                )
                .child(
                    Button::new((ElementId::from("route-delete"), alias.clone()))
                        .icon(Icon::new(IconName::Delete).small())
                        .label("删除")
                        .ghost()
                        .compact()
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_delete_route(alias.clone(), window, cx)
                        })),
                )
            });

        let mut entries_col = gpui::div().flex().flex_col().gap(px(ACCT_GAP));
        if section.accounts.is_empty() {
            // A dashed placeholder row for an entry-less route.
            entries_col = entries_col.child(
                gpui::div()
                    .h(px(ACCT_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(10.))
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("此路由还没有模型"),
            );
        }
        for (index, account) in section.accounts.iter().enumerate() {
            entries_col = entries_col.child(self.stage_entry(
                route,
                index,
                section.accounts.len(),
                account,
                pulse,
                cx,
            ));
        }
        let entries_col =
            entries_col.on_drop(cx.listener(move |this, drag: &StageEntryDrag, _, cx| {
                this.finish_stage_drag(drag, cx)
            }));
        let mut section = gpui::div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(SEC_INNER_GAP))
            .child(header)
            .child(entries_col);
        if let Some((drop_alias, slot)) = &self.stage_drop
            && drop_alias == &route.alias
        {
            // Overlay line, no layout shift: rows (and the wire endpoints
            // computed from them) stay exactly where they are.
            let line_y =
                SEC_HEAD_H + SEC_INNER_GAP + *slot as f32 * (ACCT_H + ACCT_GAP) - ACCT_GAP / 2.;
            section = section.child(
                gpui::div()
                    .absolute()
                    .left(px(0.))
                    .right(px(0.))
                    .top(px(line_y - 1.))
                    .h(px(2.))
                    .rounded_full()
                    .bg(accent),
            );
        }
        section
    }

    /// One entry row of a route section: serving dot, account head, model chip,
    /// constraints. Manage mode (self.manage) adds ▲▼ on the left and ✕ on the
    /// right — all inside the fixed row height, so the wire geometry never
    /// shifts.
    fn stage_entry(
        &self,
        route: &RouteRow,
        index: usize,
        count: usize,
        account: &StageAccount,
        pulse: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let manage = self.manage;
        let busy = self.manage_write_in_flight;
        let cooling = account.cool_until.is_some();
        let disabled = account.stopped.is_some();
        let mut dot_color = if account.live {
            theme.foreground
        } else if cooling {
            theme.warning
        } else {
            theme.muted_foreground
        };
        if account.live {
            dot_color.a = 0.45 + 0.55 * pulse;
        }
        let caption_color = if account.live {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let row = gpui::div()
            .h(px(ACCT_H))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .bg(theme.popover)
            .border_1()
            .border_color(if account.live {
                theme.foreground
            } else if cooling {
                theme.warning
            } else {
                theme.border
            })
            .rounded(px(10.))
            .overflow_hidden()
            // A fully rested or stopped entry is skipped by dispatch — dim it
            // so the stage reads as "present but out of rotation".
            .opacity(if (cooling || disabled) && !account.live {
                0.55
            } else {
                1.0
            });
        let row = if manage {
            let alias_up = route.alias.clone();
            let alias_down = route.alias.clone();
            let drag_alias = route.alias.clone();
            let move_alias = route.alias.clone();
            let drag_label = format!("{} · {}", account.head, account.model);
            let row = row
                .on_drag_move(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<StageEntryDrag>, _, cx| {
                        // Drag-move listeners fire window-wide while a matching
                        // drag is active (no hit-testing) — only this row's own
                        // bounds may claim the drop site.
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        // Cross-route dragging is out of scope.
                        if event.drag(cx).alias != move_alias {
                            return;
                        }
                        let after = event.event.position.y > event.bounds.center().y;
                        // Cloned per qualifying move: the listener is Fn (it
                        // fires on every mouse move of the drag).
                        let next = Some((move_alias.clone(), index + usize::from(after)));
                        if this.stage_drop != next {
                            this.stage_drop = next;
                            cx.notify();
                        }
                    },
                ))
                .when(!busy, |row| {
                    row.child(
                        gpui::div()
                            .id(ElementId::from(gpui::SharedString::from(format!(
                                "entry-grip-{}-{index}",
                                route.alias
                            ))))
                            .cursor_grab()
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .hover(|this| this.text_color(theme.foreground))
                            .child(Icon::new(IconName::GripVertical).small())
                            .on_drag(
                                StageEntryDrag {
                                    alias: drag_alias,
                                    from: index,
                                    label: drag_label,
                                },
                                |drag, _, _, cx| {
                                    cx.new(|_| super::DragGhost {
                                        text: drag.label.clone(),
                                    })
                                },
                            ),
                    )
                });
            row.child(
                Button::new(ElementId::from(gpui::SharedString::from(format!(
                    "entry-up-{}-{index}",
                    route.alias
                ))))
                .icon(Icon::new(IconName::ArrowUp).small())
                .ghost()
                .compact()
                .disabled(busy || index == 0)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.move_entry(alias_up.clone(), index, true, cx)
                })),
            )
            .child(
                Button::new(ElementId::from(gpui::SharedString::from(format!(
                    "entry-down-{}-{index}",
                    route.alias
                ))))
                .icon(Icon::new(IconName::ArrowDown).small())
                .ghost()
                .compact()
                .disabled(busy || index + 1 == count)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.move_entry(alias_down.clone(), index, false, cx)
                })),
            )
        } else {
            row
        };
        let row = row
            .child(
                gpui::div()
                    .w(px(8.))
                    .h(px(8.))
                    .flex_shrink_0()
                    .rounded_full()
                    .border_1()
                    .border_color(dot_color)
                    .when(account.live || cooling || disabled, |dot| dot.bg(dot_color)),
            )
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .max_w(px(220.))
                    .overflow_hidden()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(account.head.clone()),
            )
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .max_w(px(220.))
                    .overflow_hidden()
                    .font_family("Menlo")
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .bg(theme.secondary)
                    .rounded(px(5.))
                    .px(px(5.))
                    .child(account.model.clone()),
            )
            .when(!account.extras.is_empty(), |row| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(account.extras.clone()),
                )
            })
            .when_some(account.cool_until.as_ref(), |row, until| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.warning)
                        .child(until.clone()),
                )
            })
            .when_some(account.stopped.filter(|_| !account.live), |row, stopped| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(stopped.label()),
                )
            })
            // Nothing extra here: an entry is one account on one model, so the
            // row carries no tally — its own marker says why it cannot
            // dispatch, and the cooling marker says when it can again.
            .child(gpui::div().flex_1())
            .when_some(account.caption, |row, caption| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(caption_color)
                        .child(caption),
                )
            })
            .when(account.caption.is_none() && !cooling && !disabled, |row| {
                // The serving caption's idle counterpart: the latest successful call,
                // in the same slot (传输中 → lands → the time appears in place).
                row.when_some(account.last_success, |row, ok| {
                    let local = ok.with_timezone(&crate::ui_tz::display_tz());
                    let now = chrono::Utc::now().with_timezone(&crate::ui_tz::display_tz());
                    let stamp = if local.date_naive() == now.date_naive() {
                        format!("{}", local.format("%H:%M"))
                    } else if local.date_naive()
                        == now.date_naive().pred_opt().unwrap_or(local.date_naive())
                    {
                        format!("昨天 {}", local.format("%H:%M"))
                    } else {
                        format!("{}", local.format("%m-%d %H:%M"))
                    };
                    row.child(
                        gpui::div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(stamp),
                    )
                })
            });
        if manage {
            let alias = route.alias.clone();
            row.child(
                Button::new(ElementId::from(gpui::SharedString::from(format!(
                    "entry-del-{}-{index}",
                    route.alias
                ))))
                .icon(Icon::new(IconName::Close).small())
                .ghost()
                .compact()
                // The store requires ≥1 entry per route — the last row can only go
                // away by deleting the route itself (the section-level action).
                .disabled(busy || count == 1)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_remove_entry(alias.clone(), index, window, cx)
                })),
            )
        } else {
            row
        }
    }
}

/// Paint the stage wires: resting wires are solid bezier strokes; live wires
/// get a full-length faint underlay of the same bezier (the wire reads as a
/// continuous line — a 3-on/5-off dash alone is 62% empty and the hub's
/// fan-out turns to mush) with the animated dash flow on top (3 on / 5 off,
/// offset advancing with `phase`). Each wire
/// carries its own colour (client hue on caller wires, accent/border on hub
/// wires).
fn paint_wires(
    bounds: Bounds<gpui::Pixels>,
    wires: &Rc<Vec<Wire>>,
    window: &mut Window,
    phase: f32,
) {
    for wire in wires.iter() {
        // Cubic control points: 28px horizontal handles
        // (`C x1+28 y1, x2-28 y2, x2 y2`).
        let (x1, y1, x2, y2) = (wire.x1, wire.y1, wire.x2, wire.y2);
        let (c1x, c1y, c2x, c2y) = (x1 + 28., y1, x2 - 28., y2);
        let at = |t: f32| {
            let u = 1. - t;
            (
                u * u * u * x1 + 3. * u * u * t * c1x + 3. * u * t * t * c2x + t * t * t * x2,
                u * u * u * y1 + 3. * u * u * t * c1y + 3. * u * t * t * c2y + t * t * t * y2,
            )
        };
        let origin = bounds.origin;
        let solid = |window: &mut Window, color: Hsla| {
            let mut builder = PathBuilder::stroke(px(1.5));
            let (sx, sy) = at(0.);
            builder.move_to(point(origin.x + px(sx), origin.y + px(sy)));
            builder.cubic_bezier_to(
                point(origin.x + px(x2), origin.y + px(y2)),
                point(origin.x + px(c1x), origin.y + px(c1y)),
                point(origin.x + px(c2x), origin.y + px(c2y)),
            );
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        };
        if !wire.live {
            solid(window, wire.color);
            continue;
        }
        // Live: faint continuous underlay, then the flowing dashes on top.
        let mut underlay = wire.color;
        underlay.a = 0.3;
        solid(window, underlay);
        // Dash walk: sample the bezier, emit one sub-path per "on" run (3px on /
        // 5px off, the run advancing as `phase` grows → flow toward the target).
        // Samples scale with the wire's length bound so steep wires (tall stages)
        // keep ~1px steps and no 3px on-window falls between samples.
        let samples = (((x2 - x1).abs() + (y2 - y1).abs()) as usize).clamp(32, 512);
        let points: Vec<(f32, f32)> = (0..=samples)
            .map(|i| at(i as f32 / samples as f32))
            .collect();
        let mut arc = 0.;
        let mut run: Vec<gpui::Point<gpui::Pixels>> = Vec::new();
        let on = |s: f32| (s - phase).rem_euclid(8.) < 3.;
        let flush = |run: &mut Vec<gpui::Point<gpui::Pixels>>, window: &mut Window| {
            if run.len() >= 2 {
                let mut builder = PathBuilder::stroke(px(1.5));
                builder.move_to(run[0]);
                for p in &run[1..] {
                    builder.line_to(*p);
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, wire.color);
                }
            }
            run.clear();
        };
        for (index, &(ax, ay)) in points.iter().take(samples).enumerate() {
            let (bx, by) = points[index + 1];
            let seg = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
            if on(arc) {
                run.push(point(origin.x + px(ax), origin.y + px(ay)));
            } else {
                flush(&mut run, window);
            }
            arc += seg;
        }
        // Emit the terminal sample too, or every dash stops one step short of the
        // node edge (and an on-window covering the tail would vanish).
        if on(arc) {
            let (ex, ey) = points[samples];
            run.push(point(origin.x + px(ex), origin.y + px(ey)));
        }
        flush(&mut run, window);
    }
}
