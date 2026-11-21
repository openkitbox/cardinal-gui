//! The routes page's reference-shaped view: a live summary bar, a left chain
//! (clients → gateway) and a right timeline of route groups. All displayed
//! numbers are derived from the current snapshot; route management (CRUD, the
//! editor, manage mode) lives in the parent module, this module owns the view
//! and its collapse/focus/drag interactions.

use std::collections::HashSet;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use gpui::{
    App, AppContext as _, Context, Div, ElementId, Hsla, InteractiveElement, ParentElement,
    StatefulInteractiveElement, Styled, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Sizable};

use gateway::store::{ActiveWindow, ModelSuccessRow, RestRow, RouteRow, RouteStrategy};

use super::{RouteAccount, RoutesData, RoutesPage, stats};
use crate::activity::{client_hue, named_client};
use crate::pages::LoadState;

const CLIENT_LINGER: std::time::Duration = std::time::Duration::from_secs(300);
/// The single window behind the request-per-minute and average-latency figures.
const WINDOW: Duration = Duration::from_secs(60);
/// Left chain width, from the reference layout.
const CHAIN_W: f32 = 218.;
/// Entry rows keep a fixed height so the drag insertion line needs no measuring.
const ENTRY_H: f32 = 40.;
const ENTRY_GAP: f32 = 7.;
/// Width of the distribution bar track in the left chain.
const BAR_W: f32 = 70.;

/// Human-readable window text: 工作日/周…/HH:MM~HH:MM (whole-day with days
/// shows only the days).
fn describe_window(window: &ActiveWindow) -> String {
    let days = window
        .days
        .as_ref()
        .filter(|days| !days.is_empty())
        .map(|days| {
            // 1..=5 → "工作日" (the common case), otherwise "周一"-style.
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

/// One client node of the chain, derived from a caller key's label.
pub(super) struct StageClient {
    /// Display name (label minus the `cardinal-` prefix; 本机 for loopback).
    name: String,
    /// The last client-sent model id (node subtitle).
    sub: String,
    /// Last request activity — the linger clock. Live clients never expire.
    last_seen: std::time::Instant,
    /// A call of this client is in flight (its dot pulses).
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
        let utc_now = Timestamp::now();
        let seeded_from = self.clients.len();
        // recent is newest-first; iterate in reverse for chronological node order.
        for request in data.recent.iter().rev() {
            let age = utc_now
                .duration_since(request.ts)
                .try_into()
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

    /// Fold the feed's current live-call snapshot into the client registry and
    /// start the dot pulse when anything is in flight. Only the visible page
    /// calls this (from activity events) plus explicit reloads, which restore
    /// the snapshot after a hidden tab paused updates.
    pub(super) fn sync_live_clients(&mut self, cx: &mut Context<Self>) {
        let live_calls: Vec<(String, String)> = self
            .feed
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
        self.sync_clients(live_calls, cx);
        if !idle {
            self.arm_anim(cx);
        }
    }

    /// Fold the live-call snapshot into the client registry: live callers
    /// upsert (name derived from the label's `cardinal-` suffix), clients whose
    /// last call just settled start their linger clock, expired ones drop out.
    /// Any caller appears — routed or direct — on the left chain.
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

    /// One 66ms beat of the serving-dot pulse; re-arms itself while the feed is
    /// non-idle. The phase also drives the small alpha pulse of a live dot.
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
                if !page.active {
                    page.anim_armed = false;
                    return;
                }
                page.anim_armed = false;
                page.phase = (page.phase + 0.75) % 8.0;
                cx.notify();
                if !page.feed.read(cx).is_idle() {
                    page.arm_anim(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Focus a client card. Highlight only: routes are gateway-level and every
    /// client shares them, so this never filters the right-hand groups.
    fn focus_client(&mut self, name: String, cx: &mut Context<Self>) {
        self.focused_client = Some(name);
        cx.notify();
    }

    /// Expand/collapse one route group. View state only — entries, indices and
    /// drag targets stay backed by the authoritative route snapshot.
    fn toggle_group(&mut self, alias: String, cx: &mut Context<Self>) {
        if let Some(index) = self.collapsed.iter().position(|item| item == &alias) {
            self.collapsed.remove(index);
        } else {
            self.collapsed.push(alias);
        }
        cx.notify();
    }
}

/// One client card of the left chain.
struct StageSource {
    name: String,
    sub: String,
    live: bool,
    hue: Hsla,
    /// Requests from this client in the shared 60-second window.
    rate: usize,
}

/// One row of a route group: a route entry, i.e. one account on one model.
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
    /// This entry is serving a live call right now.
    live: bool,
    /// Cooldown recovery caption; Some = the entry rests.
    cool_until: Option<String>,
    /// This entry cannot dispatch: deleted/disabled/archived account, disabled
    /// provider, or a model the account does not serve.
    stopped: Option<Stopped>,
    /// Operator pause: the entry stays in the route but dispatch skips it.
    paused: bool,
    /// Newest successful request time for this account on this model.
    last_success: Option<Timestamp>,
    /// Newest recorded total latency for this (account, model) in the ledger.
    latency_ms: Option<i32>,
}

/// One in-flight call of this route, as the entry aggregation reads it.
struct LiveCall<'call> {
    /// The account serving the call (None = not yet routed to one).
    key_id: Option<i64>,
    /// The upstream model, in its clipped wire form.
    model: &'call str,
    /// Phase caption (连接中/传输中/接收中).
    phase: &'static str,
    /// The call has seen its first byte, so it outranks one still connecting.
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
    /// archive stands, whatever its own enabled flag says.
    Archived,
    /// The account is fine but its provider is disabled, so dispatch builds no
    /// candidate from it.
    ProviderDisabled,
    /// The account is dispatchable but does not serve this entry's model.
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
/// dispatch; the order only decides which one to show: an archived account
/// cannot be restored by flipping its own flag, so its own state is the useful
/// answer before its provider's.
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
/// An entry IS one account on one model, so every fact here belongs to that
/// single account. Pure: every input is a read the caller already has.
#[derive(Debug, Default, PartialEq)]
struct EntryState {
    /// Phase caption of the call serving this entry (None = idle).
    serving: Option<&'static str>,
    /// The entry cannot dispatch (deleted account, disabled account or
    /// provider, or a model the account does not serve).
    stopped: Option<Stopped>,
    /// The moment dispatch may use this entry again, while its cooldown is
    /// still in force at `now`.
    cool_until: Option<Timestamp>,
    /// Newest success of this account on this model.
    last_success: Option<Timestamp>,
}

/// Aggregate one entry's state over the single account it names.
fn entry_state(
    account: Option<&RouteAccount>,
    model: &str,
    calls: &[LiveCall<'_>],
    rests: &[RestRow],
    success: &[ModelSuccessRow],
    now: Timestamp,
) -> EntryState {
    let stopped = entry_stopped(account, model);
    let Some(account) = account else {
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
        // The gateway filters expired rows only when it builds the document,
        // so an over-the-hill rest must not read as resting here.
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
    // Ties keep feed order, so the choice is stable across renders.
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

/// The 冷却至 marker of a cooling entry: time only when the recovery falls on
/// the display day of `now`, otherwise with its date.
#[must_use]
fn cool_until_label(until: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let local = until.to_zoned_datetime(tz.clone());
    let today = now.to_zoned_datetime(tz.clone()).date();
    if local.date() == today {
        format!("冷却至 {}", local.strftime("%H:%M"))
    } else {
        format!("冷却至 {}", local.strftime("%m-%d %H:%M"))
    }
}

/// Short activity stamp in the display timezone, shared by group summaries and
/// entry rows so both name the same moment the same way.
#[must_use]
fn short_time(at: Timestamp, now: Timestamp, tz: &TimeZone) -> String {
    let local = at.to_zoned_datetime(tz.clone());
    let today = now.to_zoned_datetime(tz.clone()).date();
    if local.date() == today {
        local.strftime("%H:%M").to_string()
    } else if local.date() == today.yesterday().unwrap_or(today) {
        format!("昨天 {}", local.strftime("%H:%M"))
    } else {
        local.strftime("%m-%d %H:%M").to_string()
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

/// One route group of the timeline.
struct StageSection {
    context_window: Option<i64>,
    accounts: Vec<StageAccount>,
}

/// Everything the view derives from the snapshot for one render.
struct StageModel {
    sources: Vec<StageSource>,
    sections: Vec<StageSection>,
    /// Requests from all clients in the shared window.
    rate: usize,
    /// Mean latency in that window, if any request recorded one.
    average: Option<f64>,
    /// Distinct provider names among all accounts in the snapshot.
    providers: usize,
    /// The render's single clock read, shared by every time label.
    now: Timestamp,
}

/// Borrowed view inputs for one route group, packed so `cx` stays the only
/// other argument to [`RoutesPage::stage_group`].
struct StageGroupInputs<'a> {
    route: &'a RouteRow,
    section: &'a StageSection,
    manage: bool,
    pulse: f32,
    now: Timestamp,
    tz: &'a TimeZone,
}

/// Borrowed view inputs for one entry row, packed for the same reason.
struct StageEntryInputs<'a> {
    route: &'a RouteRow,
    index: usize,
    count: usize,
    account: &'a StageAccount,
    share: Option<u16>,
    pulse: f32,
    now: Timestamp,
    tz: &'a TimeZone,
}

impl RoutesPage {
    /// Resolve the view: summary metrics, the client chain and one section per
    /// route with its entries in dispatch order. None when there are no routes
    /// (the parent renders its empty card).
    fn stage_model(&self, data: &RoutesData, cx: &App) -> Option<StageModel> {
        if data.routes.is_empty() {
            return None;
        }
        // One clock read for cooldown expiry, the 60-second window and every
        // time label; the display offset is read once alongside it.
        let now = Timestamp::now();
        let tz = crate::ui_tz::display_tz();
        let feed = self.feed.read(cx);
        // Wire routes are clipped to 64 bytes; compare aliases in that form so
        // a long alias still matches its live calls.
        let live: Vec<&crate::activity::LiveAttempt> = feed
            .live()
            .filter(|attempt| attempt.route.is_some())
            .collect();

        // Derive the 60-second window with the same pure helpers the summary
        // uses, so the rate, the average and each client's rate cannot drift.
        let rate = stats::window_count(&data.recent, now, WINDOW);
        let average = stats::window_average_seconds(&data.recent, now, WINDOW);
        let providers = data
            .accounts
            .iter()
            .map(|account| account.provider_name.as_str())
            .collect::<HashSet<&str>>()
            .len();

        let mut sources: Vec<StageSource> = self
            .clients
            .iter()
            .map(|client| StageSource {
                name: client.name.clone(),
                sub: client.sub.clone(),
                live: client.live,
                hue: client_hue(&client.name),
                rate: stats::window_requests(&data.recent, now, WINDOW)
                    .filter(|request| {
                        named_client(request.agent.as_deref(), request.caller_key.as_deref())
                            == client.name
                    })
                    .count(),
            })
            .collect();
        if sources.is_empty() {
            sources.push(StageSource {
                name: "客户端".to_string(),
                sub: "尚无请求".to_string(),
                live: false,
                hue: client_hue("客户端"),
                rate: 0,
            });
        }

        let sections = data
            .routes
            .iter()
            .map(|route| {
                let alias = crate::activity::clipped(&route.alias);
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
                            .map(|until| cool_until_label(until, now, &tz));
                        let latency_ms = data.recent.iter().find_map(|request| {
                            if request.provider_key_id != Some(entry.key_id) {
                                return None;
                            }
                            let served =
                                request.served_model.as_deref().or(request.model.as_deref());
                            (served == Some(entry.model.as_str())).then_some(request.total_ms)?
                        });
                        StageAccount {
                            head,
                            model: entry.label.clone().unwrap_or_else(|| entry.model.clone()),
                            extras,
                            caption: state.serving,
                            live: state.serving.is_some(),
                            cool_until,
                            stopped: state.stopped,
                            paused: entry.paused,
                            last_success: state.last_success,
                            latency_ms,
                        }
                    })
                    .collect();
                StageSection {
                    context_window: route.context_window,
                    accounts,
                }
            })
            .collect();

        Some(StageModel {
            sources,
            sections,
            rate,
            average,
            providers,
            now,
        })
    }

    /// The single card: summary bar above a 218px left chain and the right
    /// timeline of route groups. In manage mode section heads carry 编辑/删除
    /// and each entry row its ▲▼/暂停/✕.
    pub(super) fn stage_card(&self, data: &RoutesData, cx: &mut Context<Self>) -> Option<Div> {
        let model = self.stage_model(data, cx)?;
        let tz = crate::ui_tz::display_tz();
        // Copied before the mutable `cx` calls below: the card surface keeps
        // these two colours while the right column builds its own elements.
        let card_border = cx.theme().border;
        let card_bg = cx.theme().popover;
        let theme = cx.theme();
        let manage = self.manage;
        let pulse = (self.phase / 8. * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        let totals = stats::route_totals(&data.routes);

        let stat = |label: &'static str, value: String, color: Hsla| {
            gpui::div()
                .flex()
                .items_center()
                .gap(px(5.))
                .child(label.to_string())
                .child(gpui::div().text_color(color).child(value))
        };
        let legend = |label: &'static str, color: Hsla| {
            gpui::div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(gpui::div().w(px(7.)).h(px(7.)).rounded(px(2.)).bg(color))
                .child(label.to_string())
        };
        let smart: Hsla = gpui::rgb(0x0969da).into();
        let weighted: Hsla = gpui::rgb(0x8250df).into();
        let alternate: Hsla = gpui::rgb(0x0f766e).into();
        let summary = gpui::div()
            .flex()
            .items_center()
            .gap(px(18.))
            .px(px(16.))
            .py(px(10.))
            .text_size(px(11.5))
            .text_color(theme.muted_foreground)
            .child(stat(
                "客户端",
                self.clients.len().to_string(),
                theme.foreground,
            ))
            .child(stat("模型组", totals.groups.to_string(), theme.foreground))
            .child(stat("路由项", totals.entries.to_string(), theme.foreground))
            .child(stat("已暂停", totals.paused.to_string(), theme.warning))
            .child("策略：每个模型独立配置主备与权重")
            .child(gpui::div().flex_1())
            .child(legend("智能", smart))
            .child(legend("加权", weighted))
            .child(legend("交替", alternate));

        // Left chain: client cards → 汇聚到网关 → cardinal → live meta →
        // distribution. The vertical line/dots are narrow divs, not wire canvas.
        let client_count = self.clients.len();
        let hub_online = !self.hub_addr.is_empty();
        let average = model
            .average
            .map_or_else(|| "—".to_string(), |seconds| format!("{seconds:.1}s"));
        let clients = gpui::div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(5.))
            .pl(px(17.))
            .child(
                gpui::div()
                    .absolute()
                    .left(px(4.))
                    .top(px(12.))
                    .bottom(px(1.))
                    .w(px(1.))
                    .bg(theme.border),
            )
            .children(model.sources.iter().enumerate().map(|(index, source)| {
                // View-only default: the first rendered client carries the
                // reference's selected look until the user picks another one.
                let selected = self.focused_client.as_deref() == Some(source.name.as_str())
                    || (self.focused_client.is_none() && index == 0);
                let initial = source
                    .name
                    .chars()
                    .next()
                    .map_or_else(|| "?".to_string(), |ch| ch.to_string());
                let card = gpui::div()
                    .id(ElementId::from(gpui::SharedString::from(format!(
                        "route-client-{}",
                        source.name
                    ))))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .bg(if selected {
                        theme.secondary
                    } else {
                        theme.popover
                    })
                    .cursor_pointer()
                    .child(
                        gpui::div()
                            .absolute()
                            .left(px(-16.))
                            .top(px(14.))
                            .w(px(7.))
                            .h(px(7.))
                            .rounded_full()
                            .border_1()
                            .border_color(if selected { theme.accent } else { theme.border })
                            .bg(if selected {
                                theme.accent
                            } else {
                                let mut hue = source.hue;
                                if source.live {
                                    hue.a = 0.45 + 0.55 * pulse;
                                }
                                hue
                            }),
                    )
                    .child(
                        gpui::div()
                            .w(px(19.))
                            .h(px(19.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .bg(source.hue)
                            .text_size(px(8.))
                            .text_color(theme.foreground)
                            .justify_center()
                            .child(initial),
                    )
                    .child(
                        gpui::div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                gpui::div()
                                    .text_size(px(11.5))
                                    .text_ellipsis()
                                    .text_color(theme.foreground)
                                    .child(source.name.clone()),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(10.))
                                    .text_ellipsis()
                                    .text_color(theme.muted_foreground)
                                    .child(source.sub.clone()),
                            ),
                    )
                    .child(
                        gpui::div()
                            .ml_auto()
                            .whitespace_nowrap()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(format!("{}/min", source.rate)),
                    );
                let name = source.name.clone();
                card.on_click(
                    cx.listener(move |this, _, _, cx| this.focus_client(name.clone(), cx)),
                )
            }));

        let max_entries = stats::route_distribution(&data.routes)
            .iter()
            .map(|(_, count)| *count)
            .max()
            .unwrap_or(0);
        let distribution = gpui::div().flex().flex_col().gap(px(7.)).children(
            stats::route_distribution(&data.routes)
                .into_iter()
                .map(|(alias, count)| {
                    let width = if max_entries == 0 {
                        0.
                    } else {
                        BAR_W * count as f32 / max_entries as f32
                    };
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .child(
                            gpui::div()
                                .w(px(76.))
                                .flex_shrink_0()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(10.5))
                                .text_color(theme.muted_foreground)
                                .child(alias.to_string()),
                        )
                        .child(
                            gpui::div()
                                .w(px(BAR_W))
                                .h(px(4.))
                                .rounded_full()
                                .bg(theme.border)
                                .child(
                                    gpui::div()
                                        .h_full()
                                        .w(px(width))
                                        .rounded_full()
                                        .bg(client_hue(alias)),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_size(px(10.5))
                                .text_color(theme.foreground)
                                .child(count.to_string()),
                        )
                }),
        );

        let left = gpui::div()
            .w(px(CHAIN_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(16.))
            .py(px(18.))
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("客户端")
                    .child(
                        gpui::div()
                            .ml(px(6.))
                            .px(px(6.))
                            .rounded(px(10.))
                            .border_1()
                            .border_color(theme.border)
                            .font_family("Menlo")
                            .text_size(px(10.))
                            .text_color(theme.foreground)
                            .child(client_count.to_string()),
                    ),
            )
            .child(clients)
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .pl(px(2.))
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(gpui::div().ml(px(3.)).w(px(1.)).h(px(14.)).bg(theme.border))
                    .child("↓")
                    .child("汇聚到网关"),
            )
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .px(px(11.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.foreground)
                    .child(
                        gpui::div()
                            .w(px(26.))
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded(px(7.))
                            .bg(theme.foreground)
                            .text_color(theme.popover)
                            .justify_center()
                            .child("C"),
                    )
                    .child(
                        gpui::div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                gpui::div()
                                    .text_size(px(12.5))
                                    .text_color(theme.foreground)
                                    .child("cardinal"),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(10.5))
                                    .text_ellipsis()
                                    .text_color(theme.muted_foreground)
                                    .child(if hub_online {
                                        self.hub_addr.clone()
                                    } else {
                                        "—".to_string()
                                    }),
                            ),
                    )
                    .when(hub_online, |node| {
                        node.child(
                            gpui::div()
                                .ml_auto()
                                .text_size(px(10.))
                                .text_color(theme.success)
                                .child("在线"),
                        )
                    }),
            )
            .child(
                gpui::div()
                    .mt(px(14.))
                    .pt(px(12.))
                    .border_t_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .text_color(if hub_online {
                                theme.success
                            } else {
                                theme.muted_foreground
                            })
                            .child(gpui::div().w(px(6.)).h(px(6.)).rounded_full().bg(
                                if hub_online {
                                    theme.success
                                } else {
                                    theme.muted_foreground
                                },
                            ))
                            .child(if hub_online {
                                "网关已连接"
                            } else {
                                "网关未连接"
                            }),
                    )
                    .child(format!("合计 {} 请求/分 · 平均 {}", model.rate, average))
                    .child(format!("上游 {} 个供应商", model.providers)),
            )
            .child(
                gpui::div()
                    .mt(px(18.))
                    .pt(px(14.))
                    .border_t_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        gpui::div()
                            .text_size(px(10.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("路由分布"),
                    )
                    .child(distribution),
            );

        // Right timeline: a thin vertical line with one card per route group.
        let mut right = gpui::div()
            .flex_1()
            .min_w_0()
            .relative()
            .py(px(16.))
            .pl(px(30.))
            .pr(px(18.))
            .on_drag_move(
                cx.listener(|this, _: &gpui::DragMoveEvent<StageEntryDrag>, _, cx| {
                    // Capture order is outer→inner: clear first, then the row
                    // under the cursor claims its section's drop site.
                    if this.stage_drop.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::div()
                    .absolute()
                    .left(px(14.))
                    .top(px(28.))
                    .bottom(px(32.))
                    .w(px(1.))
                    .bg(theme.border),
            );
        for (route, section) in data.routes.iter().zip(model.sections.iter()) {
            right = right.child(self.stage_group(
                StageGroupInputs {
                    route,
                    section,
                    manage,
                    pulse,
                    now: model.now,
                    tz: &tz,
                },
                cx,
            ));
        }

        Some(
            gpui::div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .rounded_lg()
                .border_1()
                .border_color(card_border)
                .bg(card_bg)
                .overflow_hidden()
                .child(summary)
                .child(gpui::div().flex().child(left).child(right)),
        )
    }

    /// One route group: a clickable head (chevrons, identity, strategy chip,
    /// real-data summary) over its entry rows. Manage mode appends 编辑/删除
    /// next to the head, outside its collapse click target.
    fn stage_group(&self, inputs: StageGroupInputs<'_>, cx: &mut Context<Self>) -> Div {
        let StageGroupInputs {
            route,
            section,
            manage,
            pulse,
            now,
            tz,
        } = inputs;
        let theme = cx.theme();
        let accent = theme.accent;
        let card_bg = theme.popover;
        let collapsed = self.collapsed.iter().any(|alias| alias == &route.alias);
        let shares = stats::weighted_shares(&route.entries);
        let summary = group_summary(route, section, now, tz);
        let strategy_color: Hsla = match route.strategy {
            RouteStrategy::Smart => gpui::rgb(0x0969da).into(),
            RouteStrategy::Weighted => gpui::rgb(0x8250df).into(),
            RouteStrategy::Rotate => gpui::rgb(0x0f766e).into(),
            RouteStrategy::Ordered => theme.muted_foreground,
        };
        let strategy_label = match route.strategy {
            RouteStrategy::Smart => "智能",
            RouteStrategy::Weighted => "加权",
            RouteStrategy::Rotate => "交替",
            RouteStrategy::Ordered => "顺序",
        };
        let short: String = route
            .alias
            .chars()
            .take(2)
            .flat_map(char::to_uppercase)
            .collect();
        let alias_click = route.alias.clone();
        let mut head = gpui::div()
            .id(ElementId::from(gpui::SharedString::from(format!(
                "route-group-head-{}",
                route.alias
            ))))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(9.))
            .cursor_pointer()
            .bg(theme.secondary)
            .child(if collapsed { "▸" } else { "▾" })
            .child(
                gpui::div()
                    .w(px(20.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .bg(client_hue(&route.alias))
                    .text_size(px(9.))
                    .text_color(theme.foreground)
                    .justify_center()
                    .child(short),
            )
            .child(
                gpui::div()
                    .text_size(px(12.5))
                    .text_color(theme.foreground)
                    .child(route.alias.clone()),
            )
            .child(Tag::secondary().child(format!("{} 项", section.accounts.len())))
            .when_some(section.context_window, |this, window| {
                this.child(
                    Tag::secondary()
                        .child(format!("上下文 {}", crate::fmt::humanize_count(window))),
                )
            })
            .child(
                gpui::div()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(strategy_color)
                    .px(px(6.))
                    .py(px(1.))
                    .text_size(px(10.))
                    .text_color(strategy_color)
                    .child(strategy_label),
            )
            .child(gpui::div().flex_1())
            .child(
                gpui::div()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(summary),
            )
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_group(alias_click.clone(), cx)),
            );
        if manage {
            let busy = self.manage_write_in_flight;
            let alias = route.alias.clone();
            let edit_route = route.clone();
            head = head
                .child(
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
                );
        }

        let mut card = gpui::div()
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .overflow_hidden()
            .child(head);
        if !collapsed {
            let mut entries = gpui::div()
                .relative()
                .flex()
                .flex_col()
                .gap(px(ENTRY_GAP))
                .p(px(6.))
                .on_drop(cx.listener(move |this, drag: &StageEntryDrag, _, cx| {
                    this.finish_stage_drag(drag, cx)
                }));
            if section.accounts.is_empty() {
                entries = entries.child(
                    gpui::div()
                        .h(px(ENTRY_H))
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_1()
                        .border_color(theme.border)
                        .rounded(px(6.))
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("此路由还没有模型"),
                );
            }
            for (index, account) in section.accounts.iter().enumerate() {
                entries = entries.child(self.stage_entry(
                    StageEntryInputs {
                        route,
                        index,
                        count: section.accounts.len(),
                        account,
                        share: shares.get(index).copied(),
                        pulse,
                        now,
                        tz,
                    },
                    cx,
                ));
            }
            if let Some((drop_alias, slot)) = &self.stage_drop
                && drop_alias == &route.alias
            {
                // Fixed row height keeps the insertion line exact without
                // measuring the rows; no layout shift, no wire endpoints.
                let line_y = 6. + *slot as f32 * (ENTRY_H + ENTRY_GAP) - ENTRY_GAP / 2.;
                entries = entries.child(
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
            card = card.child(entries);
        }

        gpui::div()
            .relative()
            .mb(px(14.))
            .child(
                gpui::div()
                    .absolute()
                    .left(px(-20.))
                    .top(px(18.))
                    .w(px(9.))
                    .h(px(9.))
                    .rounded_full()
                    .border_2()
                    .border_color(accent)
                    .bg(card_bg),
            )
            .child(card)
    }

    /// One entry row. Its leading control follows the group's strategy: health
    /// and recent latency for smart, a relative weight, or an order badge.
    /// Manage mode keeps the grip, reorder, pause and remove controls plus the
    /// same drag guard.
    fn stage_entry(&self, inputs: StageEntryInputs<'_>, cx: &mut Context<Self>) -> Div {
        let StageEntryInputs {
            route,
            index,
            count,
            account,
            share,
            pulse,
            now,
            tz,
        } = inputs;
        let theme = cx.theme();
        let manage = self.manage;
        let busy = self.manage_write_in_flight;
        let cooling = account.cool_until.is_some();
        let disabled = account.stopped.is_some();
        let paused = account.paused;
        let smart = route.strategy == RouteStrategy::Smart;
        let primary = smart && index == 0;
        let smart_color: Hsla = gpui::rgb(0x0969da).into();
        // The reference marks the first smart candidate with a radio ring and
        // a faint blue wash. It is a display hint only; dispatch still ranks
        // candidates from live state.
        let radio = gpui::div()
            .w(px(13.))
            .h(px(13.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .rounded_full()
            .border(px(1.5))
            .border_color(if primary { smart_color } else { theme.border })
            .justify_center()
            .child(
                gpui::div()
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .bg(if primary { smart_color } else { theme.popover }),
            );
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

        let latency = account
            .latency_ms
            .map_or_else(|| "—".to_string(), |ms| format!("{ms}ms"));
        let leading = match route.strategy {
            RouteStrategy::Smart => {
                let state = if let Some(stopped) = account.stopped {
                    stopped.label()
                } else if cooling {
                    "冷却"
                } else {
                    "健康"
                };
                let mut color: Hsla = if account.stopped.is_some() {
                    theme.muted_foreground
                } else if cooling {
                    theme.warning
                } else {
                    theme.success
                };
                if account.live {
                    color.a = 0.45 + 0.55 * pulse;
                }
                gpui::div()
                    .w(px(104.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(10.5))
                    .text_color(color)
                    .child(gpui::div().w(px(6.)).h(px(6.)).rounded_full().bg(color))
                    .child(format!("{state} · {latency}"))
            }
            RouteStrategy::Weighted => {
                let color: Hsla = gpui::rgb(0x8250df).into();
                gpui::div()
                    .w(px(104.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(11.))
                    .text_color(color)
                    .child(
                        gpui::div()
                            .rounded(px(4.))
                            .bg(theme.secondary)
                            .px(px(6.))
                            .child(format!("{}%", share.unwrap_or(0))),
                    )
                    .child("权重")
            }
            RouteStrategy::Rotate => {
                let color: Hsla = gpui::rgb(0x0f766e).into();
                gpui::div()
                    .w(px(104.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(10.5))
                    .text_color(color)
                    .child(
                        gpui::div()
                            .w(px(16.))
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .rounded_full()
                            .bg(theme.secondary)
                            .justify_center()
                            .child((index + 1).to_string()),
                    )
                    .child(if account.live { "当前" } else { "" })
            }
            RouteStrategy::Ordered => gpui::div()
                .w(px(104.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(5.))
                .text_color(theme.accent)
                .child(
                    gpui::div()
                        .w(px(20.))
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .bg(theme.accent)
                        .text_color(theme.accent_foreground)
                        .justify_center()
                        .child((index + 1).to_string()),
                )
                .when(account.live, |this| {
                    this.child(gpui::div().text_size(px(10.)).child("当前"))
                }),
        };

        let initial = account
            .head
            .chars()
            .next()
            .map_or_else(|| "?".to_string(), |ch| ch.to_string());
        let mut row = gpui::div()
            .h(px(ENTRY_H))
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(9.))
            .bg(if primary {
                theme.accent.alpha(0.08)
            } else {
                theme.popover
            })
            .border_1()
            .border_color(if account.live {
                theme.foreground
            } else if cooling {
                theme.warning
            } else {
                theme.border
            })
            .rounded(px(6.))
            .opacity(if (cooling || disabled || paused) && !account.live {
                0.55
            } else {
                1.0
            })
            .when(smart, |row| row.child(radio))
            .child(leading)
            .child(
                gpui::div()
                    .w(px(16.))
                    .h(px(16.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .bg(client_hue(&account.head))
                    .text_size(px(8.))
                    .justify_center()
                    .child(initial),
            )
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .max_w(px(200.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .text_color(theme.foreground)
                    .child(account.head.clone()),
            )
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
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
            .when(paused, |row| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(theme.warning)
                        .child("已暂停"),
                )
            })
            .child(
                gpui::div()
                    .w(px(8.))
                    .h(px(8.))
                    .flex_shrink_0()
                    .rounded_full()
                    .border_1()
                    .border_color(dot_color)
                    .when(account.live || cooling || disabled || paused, |dot| {
                        dot.bg(dot_color)
                    }),
            )
            .child(gpui::div().flex_1())
            .when_some(account.caption, |row, caption| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(theme.foreground)
                        .child(caption),
                )
            })
            .when(account.caption.is_none() && !cooling && !disabled, |row| {
                row.when_some(account.last_success, |row, ok| {
                    row.child(
                        gpui::div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(short_time(ok, now, tz)),
                    )
                })
            })
            .when(smart && !manage, |row| {
                row.child(
                    gpui::div()
                        .flex_shrink_0()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .px(px(5.))
                        .py(px(1.))
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child("自动选择"),
                )
            });
        if manage {
            let alias_up = route.alias.clone();
            let alias_down = route.alias.clone();
            let drag_alias = route.alias.clone();
            let move_alias = route.alias.clone();
            let drag_label = format!("{} · {}", account.head, account.model);
            row = row
                .on_drag_move(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<StageEntryDrag>, _, cx| {
                        // Drag-move listeners fire window-wide while a matching
                        // drag is active — only this row's own bounds may claim
                        // the drop site.
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        // Cross-route dragging stays out of scope.
                        if event.drag(cx).alias != move_alias {
                            return;
                        }
                        let after = event.event.position.y > event.bounds.center().y;
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
                })
                .child(
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
                );
            let alias = route.alias.clone();
            let toggle_alias = route.alias.clone();
            row = row
                .child(
                    Button::new(ElementId::from(gpui::SharedString::from(format!(
                        "entry-pause-{}-{index}",
                        route.alias
                    ))))
                    .label(if paused { "恢复" } else { "暂停" })
                    .ghost()
                    .compact()
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_entry_pause(toggle_alias.clone(), index, cx)
                    })),
                )
                .child(
                    Button::new(ElementId::from(gpui::SharedString::from(format!(
                        "entry-del-{}-{index}",
                        route.alias
                    ))))
                    .icon(Icon::new(IconName::Close).small())
                    .ghost()
                    .compact()
                    // The store requires ≥1 entry per route — the last row can
                    // only go away by deleting the whole route.
                    .disabled(busy || count == 1)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_remove_entry(alias.clone(), index, window, cx)
                    })),
                );
        }
        row
    }
}

/// Right side of a group head, derived entirely from the current snapshot.
fn group_summary(
    route: &RouteRow,
    section: &StageSection,
    now: Timestamp,
    tz: &TimeZone,
) -> String {
    match route.strategy {
        RouteStrategy::Weighted => {
            let shares = stats::weighted_shares(&route.entries);
            let ratios = shares
                .iter()
                .map(|share| format!("{share}%"))
                .collect::<Vec<_>>()
                .join(" · ");
            if ratios.is_empty() {
                "权重 —".to_string()
            } else {
                format!("权重 {ratios}")
            }
        }
        RouteStrategy::Rotate => format!("轮询 · {} 个账号", route.entries.len()),
        RouteStrategy::Ordered => format!("顺序 · {} 个账号", route.entries.len()),
        RouteStrategy::Smart => {
            let latest = section
                .accounts
                .iter()
                .filter_map(|account| account.last_success.map(|at| (at, account.head.as_str())))
                .max_by_key(|(at, _)| *at);
            match latest {
                Some((at, head)) => {
                    format!("最近成功 {head} · {}", short_time(at, now, tz))
                }
                None => "最近成功 —".to_string(),
            }
        }
    }
}
