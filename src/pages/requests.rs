//! Requests page: the gateway request log, read live over the gateway's
//! HTTP admin API (`client.recent_requests`). A row expands on click to show
//! the captured request/response bodies (relay body capture, gated by
//! `[gateway].payloads`) and, on failure, the vendor error.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, AppContext, Context, Div, ElementId, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::pagination::Pagination;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme, Icon, Sizable};

use gateway::body_log::CapturedBodies;
use gateway::relay::trace::{DispatchTrace, SkipReason};
use gateway::store::RequestRow;

use crate::activity::{
    LedgerReloadAction, LedgerReloadEvent, LedgerReloadState, LedgerRevisionChange,
    LedgerRevisionObserver,
};
use crate::fmt::format_cost;
use crate::fmt::usd_microus;
use crate::pages::{LoadState, empty_card, error_card, loading_card, page_frame, refresh_button};
use crate::runtime::GatewayRuntime;

fn project_request(row: gateway::activity::PersistedRequest, id: i64) -> RequestRow {
    RequestRow {
        id,
        ts: chrono::Utc::now(),
        protocol: String::new(),
        client_protocol: None,
        effort: None,
        alias: row.route,
        provider_id: Some(row.provider_id),
        provider_key_id: Some(row.provider_key_id),
        model: Some(row.model),
        served_model: row.served_model,
        status: i32::from(row.status),
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cached_input_tokens: row.cached_input_tokens,
        cache_creation_input_tokens: None,
        total_ms: Some(row.total_ms),
        ttft_ms: row.ttft_ms,
        cost_usd: row.cost_usd,
        error: row.error,
        caller_key: row.caller_key,
        agent: None,
        request_id: row.request_id,
        key_label: None,
        user_id: row.user_id,
        login: None,
        gateway_key_id: None,
        charged_microus: row.charged_microus,
        rate_bp: None,
    }
}

/// Rows shown per page.
const PAGE_SIZE: i64 = 50;

/// Rows merged from a live persisted batch have no database row id yet: their
/// negative id is a UI-only identity, never a key for a ledger read.
fn is_live_projection(id: i64) -> bool {
    id < 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistedRefresh {
    None,
    PreviewAndReload,
    ReloadOnly,
}

fn persisted_refresh(active: bool, has_older_cursor: bool, has_preview: bool) -> PersistedRefresh {
    if !active {
        PersistedRefresh::None
    } else if has_older_cursor {
        PersistedRefresh::ReloadOnly
    } else if has_preview {
        PersistedRefresh::PreviewAndReload
    } else {
        PersistedRefresh::ReloadOnly
    }
}

/// Short display name for a stored protocol id (`openai_chat` → `chat`).
fn short_protocol(id: &str) -> &str {
    match id {
        "openai_chat" => "chat",
        "openai_responses" => "responses",
        "anthropic_messages" => "anthropic",
        other => other,
    }
}

/// Single-letter code for a stored protocol id (C=chat, A=anthropic,
/// R=responses, G=gemini, E=embeddings); unknown ids keep their full name in
/// the cell.
fn protocol_letter(id: &str) -> &str {
    match id {
        "openai_chat" => "C",
        "openai_responses" => "R",
        "anthropic_messages" => "A",
        "gemini" => "G",
        "openai_embeddings" => "E",
        other => other,
    }
}

/// The request's protocol mode as letter codes: `C→A` when the relay
/// translated (client → upstream), the single upstream letter for passthrough
/// rows and for rows predating migration 0018 (no stored client protocol).
/// The full protocol names live on the cell's hover tip.
fn protocol_label(client_protocol: Option<&str>, upstream: &str) -> String {
    match client_protocol {
        Some(client) if client != upstream => {
            format!("{}→{}", protocol_letter(client), protocol_letter(upstream))
        }
        _ => protocol_letter(upstream).to_string(),
    }
}

/// The hover tip's full text behind the letter codes: the two protocol names
/// plus the relationship (translated / passthrough / legacy row).
fn protocol_tip(client_protocol: Option<&str>, upstream: &str) -> String {
    let upstream_short = short_protocol(upstream);
    match client_protocol {
        Some(client) if client != upstream => {
            format!(
                "客户端 {} → 上游 {}（协议翻译）",
                short_protocol(client),
                upstream_short
            )
        }
        Some(_) => format!("{}（直连）", upstream_short),
        None => format!("{}（上游协议；该行早于客户端协议记录）", upstream_short),
    }
}

/// Gateway request log, cursor-paginated (older = smaller id).
pub struct RequestsPage {
    state: LoadState<Vec<RequestRow>>,
    /// Live-activity feed supplies persisted summaries; no automatic ledger
    /// reload is needed on the newest page.
    feed: gpui::Entity<crate::activity::ActivityFeed>,
    persisted_reload: LedgerReloadState,
    revision_observer: LedgerRevisionObserver,
    active: bool,
    next_local_id: i64,
    /// Cursor of the page being loaded: None = newest page, Some(id) = rows
    /// older than id. Kept across loads so a refresh re-fetches the current page.
    cursor: Option<i64>,
    /// Whether an older page exists (last load returned a full page).
    has_older: bool,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    /// Expanded row ids (keyed by row id, not the in-page index: indices repeat
    /// across pages and would collide in the hitbox map).
    expanded: HashSet<i64>,
    /// Per-row captured bodies, fetched lazily on expand and pretty-printed
    /// once at fetch time (a re-render must not re-parse 256 KiB of JSON).
    bodies: HashMap<i64, LoadState<Option<BodyView>>>,
    /// Per-row routing trace, fetched lazily on expand (same lifecycle as
    /// bodies — tied to the visible page, retried on re-expand).
    traces: HashMap<i64, LoadState<Option<DispatchTrace>>>,
    /// Per-row vendor error details, prepared off the render path on expand.
    error_details: HashMap<i64, LoadState<ErrorView>>,
}

/// Display-ready captured bodies (prettied and fenced at fetch time — a
/// re-render must not re-parse 256 KiB of JSON). `*_md` is the fenced code
/// block the TextView shows; the plain copy is what the copy button ships.
struct BodyView {
    request: std::sync::Arc<str>,
    request_md: std::sync::Arc<str>,
    request_display_lines: usize,
    response: Option<std::sync::Arc<str>>,
    response_md: Option<std::sync::Arc<str>>,
    response_display_lines: Option<usize>,
    request_truncated: bool,
    response_truncated: bool,
}

impl BodyView {
    fn from_capture(bodies: CapturedBodies) -> Self {
        let (request, request_json) = pretty_body(&bodies.request);
        let request_md = fenced_code(&request, request_json);
        let request_display_lines = request_md.lines().count().max(1);
        let response = bodies.response.map(|response| pretty_body(&response));
        let (response_md, response_display_lines) =
            response.as_ref().map_or((None, None), |(text, json)| {
                let display = fenced_code(text, *json);
                let lines = display.lines().count().max(1);
                (Some(std::sync::Arc::<str>::from(display)), Some(lines))
            });
        Self {
            request_md: request_md.into(),
            request: request.into(),
            request_display_lines,
            response_md,
            response_display_lines,
            response: response.map(|(text, _)| text.into()),
            request_truncated: bodies.request_truncated,
            response_truncated: bodies.response_truncated,
        }
    }
}

/// Display-ready vendor error details, prepared once per expanded row.
struct ErrorView {
    source: std::sync::Arc<str>,
    raw: std::sync::Arc<str>,
    display: std::sync::Arc<str>,
    display_lines: usize,
}

impl ErrorView {
    fn from_error(error: &str) -> Self {
        let (raw, json) = error_detail(error);
        let display: std::sync::Arc<str> = fenced_code(&raw, json).into();
        let display_lines = display.lines().count().max(1);
        Self {
            source: error.into(),
            raw: raw.into(),
            display,
            display_lines,
        }
    }
}

fn error_detail_cache_is_current(
    cached: &LoadState<ErrorView>,
    current_error: Option<&str>,
) -> bool {
    match (cached, current_error) {
        (LoadState::Loaded(view), Some(error)) => view.source.as_ref() == error,
        _ => false,
    }
}

impl RequestsPage {
    pub(crate) fn new(
        feed: gpui::Entity<crate::activity::ActivityFeed>,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self {
            state: LoadState::Loading,
            persisted_reload: LedgerReloadState::default(),
            revision_observer: LedgerRevisionObserver::new(
                feed.read(cx).resync(),
                feed.read(cx).persisted(),
            ),
            active: false,
            next_local_id: -1,
            cursor: None,
            has_older: false,
            generation: 0,
            expanded: HashSet::new(),
            bodies: HashMap::new(),
            traces: HashMap::new(),
            error_details: HashMap::new(),
            feed,
        };
        cx.observe(&this.feed.clone(), |page, feed, cx| {
            let (persisted, resync) = {
                let feed = feed.read(cx);
                (feed.persisted(), feed.resync())
            };
            match page.revision_observer.observe(resync, persisted) {
                LedgerRevisionChange::Resync => {
                    // Resync reloads the authoritative request page and also
                    // consumes the current persisted revision. Do not take the
                    // feed preview in this pass: resync already cleared it.
                    page.apply_reload(LedgerReloadEvent::Resync, cx);
                }
                LedgerRevisionChange::Persisted(revision) => {
                    let requests = feed.update(cx, |feed, _| feed.take_persisted_requests());
                    let refresh =
                        persisted_refresh(page.active, page.cursor.is_some(), !requests.is_empty());
                    match refresh {
                        PersistedRefresh::None => {}
                        PersistedRefresh::PreviewAndReload => {
                            page.merge_persisted_rows(requests, cx);
                            page.apply_reload(LedgerReloadEvent::PersistedBatch { revision }, cx);
                        }
                        PersistedRefresh::ReloadOnly => {
                            page.apply_reload(LedgerReloadEvent::Persisted { revision }, cx);
                        }
                    }
                }
                LedgerRevisionChange::None => {}
            }
            cx.notify();
        })
        .detach();
        this
    }

    /// The status container reports whether this tab is showing. Hidden pages
    /// never start an automatic ledger read.
    pub(crate) fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        let _ = self
            .persisted_reload
            .reduce(false, LedgerReloadEvent::TabInvalidation);
    }

    fn apply_reload(&mut self, event: LedgerReloadEvent, cx: &mut Context<Self>) {
        match self.persisted_reload.reduce(self.active, event) {
            LedgerReloadAction::None => {}
            LedgerReloadAction::ReloadNow => self.reload(cx),
            LedgerReloadAction::Schedule(token) => {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(crate::activity::PERSISTED_RELOAD_DEBOUNCE)
                        .await;
                    this.update(cx, |page, cx| {
                        if matches!(
                            page.persisted_reload
                                .reduce(page.active, LedgerReloadEvent::TimerComplete(token)),
                            LedgerReloadAction::ReloadNow
                        ) {
                            page.reload(cx);
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// (Re)load the page at the current cursor over the HTTP admin API.
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        crate::pages::begin_load(&mut self.state);
        self.persisted_reload.cancel();
        self.generation += 1;
        let generation = self.generation;
        let cursor = self.cursor;
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.recent_requests(PAGE_SIZE, cursor).await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // a newer reload is in flight; this snapshot is stale
                }
                match result {
                    Ok(rows) => {
                        // A full page may have an older one; a short page is the last.
                        page.has_older = rows.len() as i64 == PAGE_SIZE;
                        // Expansion state belongs to the visible page: rows that left
                        // the page (turn/refresh) lose it with their bodies.
                        page.expanded
                            .retain(|id| rows.iter().any(|row| row.id == *id));
                        page.bodies
                            .retain(|id, _| rows.iter().any(|row| row.id == *id));
                        page.traces
                            .retain(|id, _| rows.iter().any(|row| row.id == *id));
                        page.error_details.retain(|id, state| {
                            let Some(error) = rows
                                .iter()
                                .find(|row| row.id == *id)
                                .and_then(|row| row.error.as_deref())
                                .filter(|error| !error.is_empty())
                            else {
                                return false;
                            };
                            // A loading task does not carry its source, so discard it and
                            // let the expanded-row reconciliation start a task for the newly
                            // loaded row. Failed preparation is also discarded so re-expand
                            // can retry it.
                            !matches!(state, LoadState::Loading | LoadState::Failed(_))
                                && error_detail_cache_is_current(state, Some(error))
                        });
                        page.state = LoadState::Loaded(rows);
                        page.prepare_expanded_error_details(cx);
                    }
                    Err(error) => page.state = LoadState::Failed(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Merge a newly persisted batch into the newest cached page. No HTTP
    /// request is involved; the batch is projected once, in its display order.
    fn merge_persisted_rows(
        &mut self,
        rows: Vec<gateway::activity::PersistedRequest>,
        cx: &mut Context<Self>,
    ) {
        let LoadState::Loaded(current) = &mut self.state else {
            return;
        };
        let mut incoming: Vec<RequestRow> = Vec::new();
        for row in rows {
            let id = self.next_local_id;
            self.next_local_id = self.next_local_id.saturating_sub(1);
            incoming.push(project_request(row, id));
        }
        current.splice(0..0, incoming);
        current.truncate(PAGE_SIZE as usize);
        cx.notify();
    }

    /// Jump to the page of rows older than the last row currently shown.
    fn older_page(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(rows) = &self.state else {
            return;
        };
        let Some(oldest) = rows
            .iter()
            .rev()
            .find_map(|row| (!is_live_projection(row.id)).then_some(row.id))
        else {
            return;
        };
        self.cursor = Some(oldest);
        self.apply_reload(LedgerReloadEvent::CursorInvalidation, cx);
    }

    /// Back to the newest page.
    fn newest_page(&mut self, cx: &mut Context<Self>) {
        if self.cursor.is_none() {
            return;
        }
        self.cursor = None;
        self.apply_reload(LedgerReloadEvent::CursorInvalidation, cx);
    }

    /// Expand/collapse one row; expanding fetches the captured bodies and the
    /// routing trace unless previous fetches are still usable — a miss or a
    /// failure is retried on every re-expand (the row may have been queried
    /// mid-finalize, or the call may have failed transiently).
    fn toggle_row(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.expanded.insert(id) {
            self.expanded.remove(&id);
            cx.notify();
            return;
        }
        if !is_live_projection(id) {
            let bodies_stale = matches!(
                self.bodies.get(&id),
                None | Some(LoadState::Loaded(None) | LoadState::Failed(_))
            );
            if bodies_stale {
                self.fetch_bodies(id, cx);
            }
        }
        if is_live_projection(id) {
            // Synthetic rows have no persisted trace, just like their bodies.
            self.traces.insert(id, LoadState::Loaded(None));
        } else {
            let traces_stale = matches!(
                self.traces.get(&id),
                None | Some(LoadState::Loaded(None) | LoadState::Failed(_))
            );
            if traces_stale {
                // The request_id is on the row itself; extract it for the trace fetch.
                let request_id = match &self.state {
                    LoadState::Loaded(rows) => rows
                        .iter()
                        .find(|row| row.id == id)
                        .and_then(|row| row.request_id.clone()),
                    _ => None,
                };
                self.fetch_trace(id, request_id, cx);
            }
        }
        let error = match &self.state {
            LoadState::Loaded(rows) => rows
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.error.clone())
                .filter(|error| !error.is_empty()),
            _ => None,
        };
        if let Some(error) = error
            && matches!(
                self.error_details.get(&id),
                None | Some(LoadState::Failed(_))
            )
        {
            self.prepare_error_detail(id, error, cx);
        }
        cx.notify();
    }

    fn prepare_error_detail(&mut self, id: i64, error: String, cx: &mut Context<Self>) {
        self.error_details.insert(id, LoadState::Loading);
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_spawn(async move { ErrorView::from_error(&error) })
                .await;
            this.update(cx, |page, cx| {
                let current = page
                    .current_row_error(id)
                    .is_some_and(|error| error == prepared.source.as_ref());
                if current {
                    page.error_details.insert(id, LoadState::Loaded(prepared));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn prepare_expanded_error_details(&mut self, cx: &mut Context<Self>) {
        let expanded_ids = match &self.state {
            LoadState::Loaded(rows) => rows
                .iter()
                .filter(|row| self.expanded.contains(&row.id))
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            _ => return,
        };
        for id in expanded_ids {
            let Some(error) = self.current_row_error(id) else {
                continue;
            };
            let ready = self.error_details.get(&id).is_some_and(|state| {
                matches!(state, LoadState::Loading)
                    || error_detail_cache_is_current(state, Some(error))
            });
            if !ready {
                self.prepare_error_detail(id, error.to_owned(), cx);
            }
        }
    }

    fn current_row_error(&self, id: i64) -> Option<&str> {
        match &self.state {
            LoadState::Loaded(rows) => rows
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.error.as_deref())
                .filter(|error| !error.is_empty()),
            _ => None,
        }
    }

    /// Lazy per-row body fetch over the HTTP admin API (an async round trip
    /// since the split — spec §8 decision 6; shaped after routes.rs
    /// `fetch_row_models`).
    fn fetch_bodies(&mut self, id: i64, cx: &mut Context<Self>) {
        if is_live_projection(id) {
            // A live summary has no ledger row to ask about; it becomes
            // fetchable only after a reload replaces it with the stored row.
            return;
        }
        self.bodies.insert(id, LoadState::Loading);
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.request_bodies(id).await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            let result = cx
                .background_spawn(
                    async move { result.map(|bodies| bodies.map(BodyView::from_capture)) },
                )
                .await;
            this.update(cx, |page, cx| {
                page.bodies.insert(
                    id,
                    match result {
                        Ok(bodies) => LoadState::Loaded(bodies),
                        Err(error) => LoadState::Failed(format!("{error:#}")),
                    },
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Lazy per-row trace fetch over the HTTP admin API (same lifecycle as
    /// bodies). The trace is keyed by the row's `request_id` (`req_{n}`),
    /// which may be None on rows predating migration 0021.
    fn fetch_trace(&mut self, id: i64, request_id: Option<String>, cx: &mut Context<Self>) {
        self.traces.insert(id, LoadState::Loading);
        let Some(request_id) = request_id else {
            // Old row without a request_id — no trace was ever recorded.
            self.traces.insert(id, LoadState::Loaded(None));
            cx.notify();
            return;
        };
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.trace(&request_id).await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                page.traces.insert(
                    id,
                    match result {
                        Ok(trace) => LoadState::Loaded(trace),
                        Err(error) => LoadState::Failed(format!("{error:#}")),
                    },
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Tab-row action (rendered by the Status container): refresh.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        vec![
            refresh_button(
                "requests-refresh",
                cx.listener(|this, _, _, cx| {
                    this.apply_reload(LedgerReloadEvent::ManualReload, cx)
                }),
            )
            .into_any_element(),
        ]
    }
}

impl Render for RequestsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No header: the tab bar names the view and the refresh action lives in
        // the tab row, so the content starts immediately.
        let frame = page_frame("", "", cx);
        match &self.state {
            LoadState::Loading => frame.child(loading_card(cx)),
            LoadState::Failed(error) => frame.child(error_card(error, cx)),
            LoadState::Loaded(rows) => {
                if rows.is_empty() {
                    return frame.child(empty_card(
                        "暂无请求",
                        if self.cursor.is_some() {
                            "没有更早的请求了。"
                        } else {
                            "经 gateway 服务的请求会显示在这里。"
                        },
                        cx,
                    ));
                }
                let table = gpui::div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(requests_header(cx))
                    .children(rows.iter().map(|row| request_row(self, row, cx)));
                frame.child(table).child(pager(
                    self.cursor.is_some(),
                    self.has_older,
                    rows,
                    cx.entity(),
                    cx,
                ))
            }
        }
    }
}

/// Pager row under the table: the id range on the left, gpui-kit's Pagination
/// (compact: prev/next only) on the right. The log is cursor-paginated, so a
/// true page number and a total count don't exist — current is 1 on the
/// newest page and 2 anywhere older, and the total stretches by one when an
/// older page is known to exist, which is all the nav buttons need.
/// (prev = 较新, next = 更早.)
fn pager(
    has_newer: bool,
    has_older: bool,
    rows: &[RequestRow],
    page: gpui::Entity<RequestsPage>,
    cx: &App,
) -> Div {
    let range = match (rows.first(), rows.last()) {
        (Some(first), Some(last)) => format!("{} – {}", last.id, first.id),
        _ => String::new(),
    };
    let current = if has_newer { 2 } else { 1 };
    let total = current + usize::from(has_older);
    gpui::div()
        .flex()
        .items_center()
        .justify_between()
        .w_full()
        .child(
            gpui::div()
                .text_color(cx.theme().muted_foreground)
                .text_size(px(12.))
                .child(range),
        )
        .child(
            Pagination::new("requests-pager")
                .compact()
                .current_page(current)
                .total_pages(total)
                .on_click(move |target, _, cx| {
                    page.update(cx, |this, cx| {
                        if *target > current {
                            this.older_page(cx);
                        } else if *target < current {
                            this.newest_page(cx);
                        }
                    });
                }),
        )
}

/// One flexible text column (grows evenly with its siblings, ellipsizes).
fn flex_col(text: impl IntoElement, right: bool) -> Div {
    let cell = gpui::div()
        .flex_1()
        .min_w_0()
        .px_2()
        .py_1()
        .truncate()
        .child(text);
    if right {
        cell.flex().justify_end()
    } else {
        cell
    }
}

/// Fixed supplier column wide enough for `itoby-1/kimi-for-cofing-highspeed`.
const SUPPLIER_W: f32 = 260.;

fn supplier_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(SUPPLIER_W))
        .px_2()
        .py_1()
        .truncate()
        .child(text)
}

fn supplier_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(SUPPLIER_W))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child("供应商")
}

/// The fixed-width 模式 column, data cell (letter codes, muted — secondary
/// routing info; the full protocols show on hover).
fn protocol_cell(
    client_protocol: Option<&str>,
    upstream: &str,
    row_id: u64,
    cx: &App,
) -> gpui::Stateful<Div> {
    let tip = protocol_tip(client_protocol, upstream);
    gpui::div()
        .id(("protocol-tip", row_id))
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .truncate()
        .text_color(cx.theme().muted_foreground)
        .child(protocol_label(client_protocol, upstream))
        .tooltip(move |_, cx| {
            cx.new(|_| crate::app::StatusTip { text: tip.clone() })
                .into()
        })
}

/// The 模式 column's header cell (fixed width, matching the data cells).
fn protocol_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child("模式")
}

/// One fixed-width text column (narrow, ellipsizes — the 推理 strength and
/// similar short secondary facts that shouldn't dilute the flexible columns).
fn fixed_col(text: String, cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(56.))
        .px_2()
        .py_1()
        .truncate()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// The fixed-width column's header cell (matching [`fixed_col`]).
fn fixed_header(text: &str, cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(56.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(text.to_string())
}

/// The 推理 column's data cell: the client-asked reasoning effort as a
/// single uppercase level (L/M/H/X, `MAX` — max has no free single letter),
/// color-stepped so the expensive levels stand out first, exact word on
/// hover. `None` renders the plain dash (no effort named).
fn effort_cell(effort: Option<&str>, row_id: u64, cx: &App) -> gpui::Stateful<Div> {
    let Some(effort) = effort else {
        return fixed_col("—".into(), cx).id(("effort-none", row_id));
    };
    let (badge, color, full) = match effort {
        "max" => ("MAX", cx.theme().danger, "max"),
        "xhigh" => ("X", cx.theme().warning, "xhigh"),
        "high" => ("H", cx.theme().foreground, "high"),
        "medium" => ("M", cx.theme().accent, "medium"),
        "low" => ("L", cx.theme().muted_foreground, "low"),
        // Unknown future level: verbatim text, neutral color.
        other => (other, cx.theme().muted_foreground, other),
    };
    let tip = format!("推理强度：{full}");
    fixed_col(badge.to_string(), cx)
        .id(("effort-tip", row_id))
        .text_color(color)
        .tooltip(move |_, cx| {
            cx.new(|_| crate::app::StatusTip { text: tip.clone() })
                .into()
        })
}

/// Header row over the expandable rows (mirrors the data row widths).
fn requests_header(cx: &App) -> Div {
    let label = |text: &str, right: bool| {
        let cell = gpui::div()
            .px_2()
            .py_1()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(text.to_string());
        if right {
            cell.flex_1().min_w_0().flex().justify_end()
        } else {
            cell.flex_1().min_w_0()
        }
    };
    gpui::div()
        .flex()
        .items_center()
        .w_full()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            gpui::div()
                .flex_none()
                .w(px(90.))
                .px_2()
                .py_1()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child("时间"),
        )
        .child(protocol_header(cx))
        .child(fixed_header("推理", cx))
        .child(route_header(cx))
        .child(caller_header(cx))
        .child(user_header(cx))
        .child(supplier_header(cx))
        .child(status_header(cx))
        .child(ttft_header(cx))
        .child(speed_header(cx))
        .child(label("Token 入/出", true))
        .child(label("金额", true))
        .child(label("扣费", true))
}

/// One request row: the clickable main line (7 columns, same widths as the
/// header) plus, when expanded, the captured-bodies detail below it.
fn request_row(page: &RequestsPage, row: &RequestRow, cx: &mut Context<RequestsPage>) -> Div {
    let row_id = row.id;
    let accent = cx.theme().accent;
    let line = gpui::div()
        .id(("request-row", row_id as u64))
        .flex()
        .items_center()
        .w_full()
        .cursor_pointer()
        .hover(move |this| this.bg(accent))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_row(row_id, cx)))
        .child(
            gpui::div().flex_none().w(px(90.)).px_2().py_1().child(
                row.ts
                    .with_timezone(&crate::ui_tz::display_tz())
                    .format("%H:%M:%S")
                    .to_string(),
            ),
        )
        .child(protocol_cell(
            row.client_protocol.as_deref(),
            &row.protocol,
            row_id as u64,
            cx,
        ))
        .child(effort_cell(row.effort.as_deref(), row_id as u64, cx))
        .child(route_cell(
            row.alias.clone().unwrap_or_else(|| "（直连）".to_string()),
        ))
        // The client the request's User-Agent names (opencode/codex/…) wins over
        // caller_key (the gateway key label): the UA says who actually called,
        // the label only which key they used (activity's named_client).
        .child(caller_cell(crate::activity::named_client(
            row.agent.as_deref(),
            row.caller_key.as_deref(),
        )))
        .child(user_cell(row.login.as_deref().unwrap_or("—").to_string()))
        // 供应商列：账号 label / 实际服务的上游模型 ID，与动态同一口径
        // （`fmt::account_model`：模型 ID 去掉前导厂商命名空间）。
        .child(supplier_cell(crate::fmt::account_model(
            row.key_label.as_deref(),
            row.served_model.as_deref(),
        )))
        .child(status_cell(row.status))
        .child(ttft_cell(crate::fmt::format_ttft(row.ttft_ms)))
        .child(speed_cell(crate::fmt::format_speed(
            row.output_tokens,
            row.total_ms,
            row.ttft_ms,
        )))
        .child(flex_col(
            format!("{}/{}", tokens(row.input_tokens), tokens(row.output_tokens)),
            true,
        ))
        .child(flex_col(format_cost(row.cost_usd), true))
        .child(charged_cell(row));
    let mut wrapper = gpui::div().flex().flex_col().w_full().child(line);
    if page.expanded.contains(&row_id) {
        wrapper = wrapper.child(row_detail(page, row, cx));
    }
    wrapper
}

/// The expanded detail under one row: request/response body panels from the
/// relay's capture ring, plus the vendor error on failed rows.
fn row_detail(page: &RequestsPage, row: &RequestRow, cx: &mut Context<RequestsPage>) -> Div {
    let mut detail = gpui::div()
        .flex()
        .flex_col()
        .gap_2()
        .w_full()
        .px_2()
        .py_2()
        .border_t_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted);
    if is_live_projection(row.id) {
        detail = detail.child(note_panel(
            "请求正文",
            "实时摘要行没有记录 ID，不加载正文；刷新后可按存储记录查看",
            cx,
        ));
    } else {
        match page.bodies.get(&row.id) {
            None | Some(LoadState::Loading) => {
                detail = detail.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("加载中…"),
                );
            }
            Some(LoadState::Failed(error)) => {
                detail = detail.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            Some(LoadState::Loaded(None)) => {
                detail = detail.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "正文未捕获（[gateway].payloads 关闭、已被淘汰，或 relay 在另一进程）",
                        ),
                );
            }
            Some(LoadState::Loaded(Some(bodies))) => {
                detail = detail
                    .child(body_panel(
                        "request-body",
                        row.id,
                        "请求正文",
                        &bodies.request,
                        &bodies.request_md,
                        bodies.request_display_lines,
                        bodies.request_truncated.then_some("已截断 · 前 256KB"),
                        cx,
                    ))
                    .child(match (&bodies.response, &bodies.response_md) {
                        (Some(response), Some(response_md)) => body_panel(
                            "response-body",
                            row.id,
                            "响应正文",
                            response,
                            response_md,
                            bodies.response_display_lines.unwrap_or(1),
                            bodies
                                .response_truncated
                                .then_some("不完整（截断或含非 UTF-8 帧）"),
                            cx,
                        ),
                        _ => note_panel("响应正文", "无响应正文（请求失败）", cx),
                    });
            }
        }
    }
    // Routing trace panel (loaded lazily like the bodies).
    match page.traces.get(&row.id) {
        None | Some(LoadState::Loading) => {
            detail = detail.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("路由决策加载中…"),
            );
        }
        Some(LoadState::Failed(error)) => {
            detail = detail.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(format!("路由决策: {error}")),
            );
        }
        Some(LoadState::Loaded(None)) => {
            detail = detail.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("该请求无路由记录"),
            );
        }
        Some(LoadState::Loaded(Some(trace))) => {
            detail = detail.child(trace_panel(trace, cx));
        }
    }
    if row.error.as_deref().is_some_and(|error| !error.is_empty()) {
        match page.error_details.get(&row.id) {
            None | Some(LoadState::Loading) => {
                detail = detail.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("错误详情加载中…"),
                );
            }
            Some(LoadState::Failed(error)) => {
                detail = detail.child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(error.clone()),
                );
            }
            Some(LoadState::Loaded(error)) => {
                detail = detail.child(body_panel(
                    "request-error-detail",
                    row.id,
                    "错误",
                    &error.raw,
                    &error.display,
                    error.display_lines,
                    None,
                    cx,
                ));
            }
        }
    }
    detail
}

/// One detail panel: a title row (label + truncation note + copy button —
/// copies the truncated text as shown) OVER a bounded body. The body is a
/// TextView code block in `scrollable(true)` mode — selectable (drag, then
/// ⌘C), soft-wrapped inside the box, and VIRTUALIZED through its internal
/// ListState, so a 256KB capture lays out only the visible blocks instead of
/// the whole document every frame (a plain overflow-scroll wrapper around a
/// non-scrollable TextView did the latter and pegged the CPU on expand). Its
/// native scroll handling also keeps the wheel from leaking to the outer
/// request list (gpui-base's `scrolling_code_block_keeps_wheel_from_the_list`).
#[allow(clippy::too_many_arguments)]
fn body_panel(
    name: &'static str,
    row_id: i64,
    label: &str,
    raw: &std::sync::Arc<str>,
    display: &std::sync::Arc<str>,
    display_lines: usize,
    note: Option<&'static str>,
    cx: &mut Context<RequestsPage>,
) -> Div {
    let copy = std::sync::Arc::clone(raw);
    let copy_id = ElementId::Name(format!("{name}-copy-{row_id}").into());
    let view_id = ElementId::Name(format!("{name}-view-{row_id}").into());
    let mut title = gpui::div().flex().items_center().gap_2().child(
        gpui::div()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(label.to_string()),
    );
    if let Some(note) = note {
        title = title.child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(note),
        );
    }
    title = title.child(
        Button::new(copy_id)
            .icon(Icon::new(IconName::Copy).small())
            .ghost()
            .compact()
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.to_string()));
            })),
    );
    // The scrollable TextView fills its parent (`size_full`), so the wrapper
    // must carry a definite height. Estimate from the fenced source's line
    // count: underestimate is safe (content scrolls), overestimate only leaves
    // empty space — clamp to the same 320px the old wrapper used.
    let height = px((display_lines as f32 * 17.0 + 32.0).clamp(48.0, 320.0));
    gpui::div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .min_w_0()
        .child(title)
        .child(
            gpui::div()
                .w_full()
                .min_w_0()
                .h(height)
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(cx.theme().border)
                .child(
                    TextView::markdown(view_id, gpui::SharedString::new(display.clone()))
                        .scrollable(true),
                ),
        )
}

/// A panel that only explains why there is no body to show.
fn note_panel(label: &str, note: &str, cx: &App) -> Div {
    gpui::div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(note.to_string()),
        )
}

/// The routing-decision panel: strategy, model, and the candidate list with
/// per-candidate status and Smart reason.
fn trace_panel(trace: &DispatchTrace, cx: &App) -> Div {
    let mut panel = gpui::div().flex().flex_col().gap_2().w_full();
    // Header: model + strategy + alias
    let header = gpui::div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child("路由决策"),
        )
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "model: {} · strategy: {}",
                    trace.model, trace.strategy
                )),
        );
    let header = if let Some(alias) = &trace.alias {
        header.child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!("alias: {alias}")),
        )
    } else {
        header
    };
    panel = panel.child(header);

    // Candidate list
    let mut list = gpui::div().flex().flex_col().gap_1().w_full();
    for candidate in &trace.candidates {
        let selected = candidate.skipped.is_none();
        let icon = if selected { "✓" } else { "✗" };
        let color = if selected {
            gpui::green()
        } else {
            cx.theme().muted_foreground
        };
        let status = if selected {
            "selected".to_string()
        } else {
            skip_reason_label(candidate.skipped.as_ref())
        };
        let mut line = gpui::div()
            .flex()
            .items_center()
            .gap_2()
            .child(gpui::div().text_color(color).child(icon))
            .child(gpui::div().flex_1().min_w_0().truncate().child(format!(
                "{} / {} / {}",
                candidate.key_label, candidate.provider_name, candidate.model
            )))
            .child(
                gpui::div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            );
        if !candidate.smart_reason.is_empty() {
            line = line.child(
                gpui::div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(candidate.smart_reason.clone()),
            );
        }
        list = list.child(line);
    }
    panel = panel.child(list);
    panel
}

/// Human label for a skip reason.
fn skip_reason_label(reason: Option<&SkipReason>) -> String {
    match reason {
        Some(SkipReason::Disabled) => "disabled".to_string(),
        Some(SkipReason::WindowSkipped) => "window_skipped".to_string(),
        Some(SkipReason::Resting) => "resting".to_string(),
        Some(SkipReason::ModelNotAllowed) => "model_not_allowed".to_string(),
        Some(SkipReason::SecretSkipped) => "secret_skipped".to_string(),
        Some(SkipReason::ConcurrencyFull) => "concurrency_full".to_string(),
        Some(SkipReason::QueueTimeout) => "queue_timeout".to_string(),
        None => "ready".to_string(),
    }
}

/// Status code tag: 2xx green, 4xx amber, 5xx red, anything else neutral.
/// No failure badge: the vendor error lives in the expanded row's detail,
/// where it can actually be read, scrolled and copied.
/// Status code with color, no background — three digits, fixed narrow column.
fn status_cell(status: i32) -> Div {
    let color = match status {
        200..=299 => gpui::green(),
        400..=499 => gpui::yellow(),
        500..=599 => gpui::red(),
        _ => gpui::hsla(0.0, 0.0, 0.53, 1.0),
    };
    gpui::div()
        .flex_none()
        .w(px(48.))
        .px_2()
        .py_1()
        .flex()
        .justify_end()
        .text_color(color)
        .child(status.to_string())
}

/// The status column's header cell (fixed width, matching the data cells).
fn status_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(48.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .flex()
        .justify_end()
        .child("状态")
}

/// TTFT cell — short duration like "1.2s", fixed narrow column.
fn ttft_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(56.))
        .px_2()
        .py_1()
        .flex()
        .justify_end()
        .child(text)
}

/// The TTFT column's header cell (fixed width, matching the data cells).
fn ttft_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(56.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .flex()
        .justify_end()
        .child("首字")
}

/// Route alias cell — short names like "k3-256k", fixed narrow column.
fn route_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .truncate()
        .child(text)
}

/// The route column's header cell (fixed width, matching the data cells).
fn route_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child("路由")
}

/// Speed cell — like "12.3t/s", fixed narrow column.
fn speed_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .flex()
        .justify_end()
        .child(text)
}

/// The speed column's header cell (fixed width, matching the data cells).
fn speed_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(72.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .flex()
        .justify_end()
        .child("速度")
}

/// Caller cell — client name like "opencode", fixed narrow column.
fn caller_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(120.))
        .px_2()
        .py_1()
        .truncate()
        .child(text)
}

/// The caller column's header cell (fixed width, matching the data cells).
fn caller_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(120.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child("调用方")
}

/// User cell — the login owning the caller's gateway key, fixed narrow column.
fn user_cell(text: String) -> Div {
    gpui::div()
        .flex_none()
        .w(px(96.))
        .px_2()
        .py_1()
        .truncate()
        .child(text)
}

/// The user column's header cell (fixed width, matching the data cells).
fn user_header(cx: &App) -> Div {
    gpui::div()
        .flex_none()
        .w(px(96.))
        .px_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child("用户")
}

/// The amount debited for one attempt, `None` when the row was not billed:
/// no charge recorded (unknown usage, unpriced model, in-process caller) or
/// a zero/absent rate (an internal free user is charged 0 at rate 0 — that
/// is not revenue, so it reads like an unbilled row).
fn billed_charge(row: &RequestRow) -> Option<(i64, i64)> {
    let rate_bp = row.rate_bp.filter(|rate_bp| *rate_bp > 0)?;
    Some((row.charged_microus?, rate_bp))
}

/// The 扣费 cell: the debited USD with the rate on hover, `—` when unbilled
/// ([`billed_charge`]).
fn charged_cell(row: &RequestRow) -> gpui::Stateful<Div> {
    let Some((charged, rate_bp)) = billed_charge(row) else {
        return flex_col("—", true).id(("charged-none", row.id as u64));
    };
    let tip = format!("倍率 {}", gateway::billing::format_rate(rate_bp));
    flex_col(usd_microus(charged), true)
        .id(("charged-tip", row.id as u64))
        .tooltip(move |_, cx| {
            cx.new(|_| crate::app::StatusTip { text: tip.clone() })
                .into()
        })
}

/// Optional token count, em-dash when unmeasured.
fn tokens(value: Option<i64>) -> String {
    value.map_or_else(|| "—".to_string(), crate::fmt::humanize_count)
}

/// The tooltip body: vendor error JSON pretty-printed when it parses (most
/// 4xx/5xx bodies are `{"error":{...}}`), otherwise the raw message. The bool
/// marks JSON, so the display fence can carry the language.
fn error_detail(error: &str) -> (String, bool) {
    // The stored message is "upstream HTTP 404: {json}" — try the part after
    // the first ": " as JSON; fall back to the whole message.
    let candidate = error.split_once(": ").map_or(error, |(_, rest)| rest);
    match serde_json::from_str::<serde_json::Value>(candidate)
        .and_then(|value| serde_json::to_string_pretty(&value))
    {
        Ok(pretty) => (pretty, true),
        Err(_) => (error.to_owned(), false),
    }
}

/// `text` wrapped in a markdown code fence for TextView display. The fence is
/// one backtick longer than the longest backtick run inside the text, so a
/// body carrying fences of its own can't break out and render as markdown.
fn fenced_code(text: &str, json: bool) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    let lang = if json { "json" } else { "" };
    // One fenced block per CHUNK, not one for the whole body: the TextView's
    // virtualization is per markdown block (one list item each), so a 256KB
    // capture in a single fence would be one giant item shaped on every frame
    // and every scroll step — the expand lag and dead scrolling. Chunks keep
    // each item small; only the visible ones are laid out. The blank line
    // between chunks makes the parser treat them as separate blocks.
    text.lines()
        .collect::<Vec<_>>()
        .chunks(CODE_CHUNK_LINES)
        .map(|chunk| format!("{fence}{lang}\n{}\n{fence}\n\n", chunk.join("\n")))
        .collect()
}

/// Code-block chunk size, in source lines: small enough that a chunk lays out
/// in well under a frame, large enough that the block list stays in the
/// dozens rather than the thousands.
const CODE_CHUNK_LINES: usize = 128;

/// A captured body pretty-printed when it parses as JSON; otherwise verbatim
/// (SSE frame dumps and plain text are not JSON). The bool marks JSON, so the
/// display fence can carry the language.
fn pretty_body(text: &str) -> (String, bool) {
    match serde_json::from_str::<serde_json::Value>(text)
        .and_then(|value| serde_json::to_string_pretty(&value))
    {
        Ok(pretty) => (pretty, true),
        Err(_) => (text.to_owned(), false),
    }
}

#[cfg(test)]
mod realtime_tests {
    use super::{PersistedRefresh, persisted_refresh};
    use crate::activity::{LedgerReloadAction, LedgerReloadEvent, LedgerReloadState};

    #[test]
    fn persisted_events_preview_newest_and_reload_active_pages() {
        assert_eq!(
            persisted_refresh(false, false, true),
            PersistedRefresh::None
        );
        assert_eq!(
            persisted_refresh(true, false, true),
            PersistedRefresh::PreviewAndReload
        );
        assert_eq!(
            persisted_refresh(true, true, true),
            PersistedRefresh::ReloadOnly
        );
    }

    #[test]
    fn plain_persisted_reload_is_scheduled_on_newest_page() {
        assert_eq!(
            persisted_refresh(true, false, false),
            PersistedRefresh::ReloadOnly
        );
    }

    #[test]
    fn hidden_persisted_event_skips_reload_and_older_cursor_keeps_reload_cursor() {
        assert_eq!(
            persisted_refresh(false, false, false),
            PersistedRefresh::None
        );
        assert_eq!(
            persisted_refresh(true, true, true),
            PersistedRefresh::ReloadOnly
        );
    }

    #[test]
    fn persisted_reload_burst_coalesces_and_invalidation_cancels_stale_timer() {
        let mut state = LedgerReloadState::default();
        let first = match state.reduce(true, LedgerReloadEvent::Persisted { revision: 1 }) {
            LedgerReloadAction::Schedule(token) => token,
            action => panic!("expected scheduled reload, got {action:?}"),
        };
        assert_eq!(
            state.reduce(true, LedgerReloadEvent::Persisted { revision: 2 },),
            LedgerReloadAction::None,
        );
        assert_eq!(
            state.reduce(true, LedgerReloadEvent::ManualReload),
            LedgerReloadAction::ReloadNow,
        );
        assert_eq!(
            state.reduce(true, LedgerReloadEvent::TimerComplete(first)),
            LedgerReloadAction::None,
        );
    }
}
