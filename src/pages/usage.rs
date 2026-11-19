//! Usage page: six KPI cards (led-strip blocks: label / big value /
//! sub line, or a meter for the cache hit rate) over the trend card, with the
//! 动态 feed (`pages::feed`) below. The led-trend (metric/split
//! switches, stacked-column chart with hover tooltip, per-category ranking)
//! lives in `usage_trend`. The cards read the whole-window aggregate
//! (`usage_window`); the trend reads bucket × category aggregates
//! (`usage_trend`).

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyWindowHandle, AppContext as _, Context, Div, ElementId, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, px, relative,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::calendar::Date as PickerDate;
use gpui_kit::component::date_picker::{
    DatePicker, DatePickerEvent, DatePickerState, DateTime as PickerValue,
};
use jiff::civil::Date;
use jiff::{SignedDuration, Timestamp};

use gateway::http_client::UsageScope;
use gateway::store::{GatewayKeyRow, OWNER_LOGIN, TrendRow, TrendSplit, UsageWindow, UserRow};

use crate::activity::{
    LedgerRevisionChange, LedgerRevisionObserver, UsageReloadAction, UsageReloadEvent,
    UsageReloadState,
};
use crate::fmt::{humanize_count_overview, usd_microus};
use crate::pages::usage_trend::{TrendData, TrendGeometryCell, TrendMetric};
use crate::pages::{
    LoadState, card, empty_card, error_card, loading_card, notify_error, notify_success,
    page_frame, refresh_button,
};
use crate::runtime::GatewayRuntime;

/// The longest window the facade serves; [`UsageRange::resolve`] clamps to
/// the same span so the chart grid and the SQL cover identical instants.
const MAX_SPAN_DAYS: i64 = 370;

/// How often the 动态 feed's in-flight elapsed labels repaint: half a second
/// keeps the count-up live without polling or re-reading the ledger.
const ELAPSED_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether the overview page should arm its elapsed repaint timer: only while
/// the page is active, no timer is already armed, and the feed has a live row
/// under the page's own client scope.
fn should_arm_elapsed(active: bool, armed: bool, has_live_rows: bool) -> bool {
    active && !armed && has_live_rows
}

/// The window the page shows — the tab-row chips or the date picker's custom
/// range.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageRange {
    /// The local calendar day so far (midnight → now).
    Today,
    /// A rolling window of the last N days (`ts >= now() - N days`).
    Days(i32),
    /// A custom local-date range, both ends inclusive.
    Custom(Date, Date),
}

/// The query form of a [`UsageRange`]: `[since, until)` in UTC plus the local
/// offset the SQL and the bucket grid shift labels by — captured at resolve
/// so a load's query and chart agree even across an offset change.
#[derive(Clone, Copy)]
pub(crate) struct ResolvedRange {
    pub since: Timestamp,
    pub until: Timestamp,
    pub offset: SignedDuration,
}

impl UsageRange {
    /// Chip row: preset ranges and their labels.
    pub(super) const PRESETS: [(UsageRange, &'static str); 4] = [
        (UsageRange::Today, "今天"),
        (UsageRange::Days(7), "7 天"),
        (UsageRange::Days(30), "30 天"),
        (UsageRange::Days(90), "90 天"),
    ];

    pub(super) fn resolve(self) -> ResolvedRange {
        let tz = crate::ui_tz::display_tz();
        let now = Timestamp::now();
        let offset = crate::time::offset(now, tz.clone());
        let (since, until) = match self {
            Self::Today => (
                local_midnight(tz.clone(), crate::time::date(now, tz.clone())),
                now,
            ),
            Self::Days(days) => (now - SignedDuration::from_hours(24 * i64::from(days)), now),
            // The picker speaks inclusive dates; the query is half-open.
            Self::Custom(start, end) => (
                local_midnight(tz.clone(), start),
                local_midnight(tz, end.tomorrow().unwrap_or(end)),
            ),
        };
        ResolvedRange {
            since: since
                .max(until - SignedDuration::from_hours(24 * MAX_SPAN_DAYS))
                .min(until),
            until,
            offset,
        }
    }

    /// The empty-card period line ("今天内没有计量的请求。").
    pub(super) fn label(self) -> String {
        match self {
            Self::Today => "今天".to_string(),
            Self::Days(days) => format!("{days} 天"),
            Self::Custom(start, end) => format!("{start} 至 {end}"),
        }
    }
}

/// The UTC instant of a display-day's 00:00. The display zone is a fixed
/// offset, so DST transitions cannot skip or repeat midnight.
fn local_midnight(tz: jiff::tz::TimeZone, date: Date) -> Timestamp {
    crate::time::local_timestamp(date, jiff::civil::Time::midnight(), tz)
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Interpolate a fractional KPI value without rounding before formatting.
fn interpolated_value(from: f64, to: f64, progress: f32) -> f64 {
    let progress = f64::from(progress.clamp(0.0, 1.0));
    if progress == 1.0 {
        return to;
    }
    from + (to - from) * progress
}

/// Interpolate integer counters using fixed-point arithmetic. Converting an
/// `i64` endpoint to `f64` would skip values near the top of the integer range.
fn interpolated_integer_value(from: i64, to: i64, progress: f32) -> i64 {
    const SCALE: i128 = 1_000_000;
    let progress = i128::from((progress.clamp(0.0, 1.0) * SCALE as f32).round() as i64);
    let delta = i128::from(to) - i128::from(from);
    let scaled = i128::from(from) * SCALE + delta * progress;
    let rounded = if scaled >= 0 {
        (scaled + SCALE / 2) / SCALE
    } else {
        (scaled - SCALE / 2) / SCALE
    };
    rounded as i64
}

/// Numeric values shown by the overview cards. Keeping only scalar display
/// inputs avoids retaining or cloning the complete usage snapshot for an
/// animation.
#[derive(Clone, Copy, Debug, PartialEq)]
struct KpiValues {
    tokens: i64,
    requests: i64,
    charged_microus: i64,
    cache_rate: Option<f64>,
    cost_usd: Option<f64>,
    speed: Option<f64>,
}

const KPI_ANIMATION_DURATION: std::time::Duration = std::time::Duration::from_millis(450);

/// Owns the one page-level animation clock. Rendering samples this state
/// directly instead of delegating timing to an element, so an element being
/// hidden or remounted cannot restart the animation.
#[derive(Default)]
struct KpiAnimation {
    from: Option<KpiValues>,
    target: Option<KpiValues>,
    started: Option<std::time::Instant>,
    displayed: Option<KpiValues>,
}

impl KpiAnimation {
    fn update(
        &mut self,
        target: Option<KpiValues>,
        now: std::time::Instant,
        animate: bool,
        reduce_motion: bool,
    ) {
        let Some(target) = target else {
            self.from = None;
            self.target = None;
            self.started = None;
            self.displayed = None;
            return;
        };
        if !animate || reduce_motion || self.target.is_none() {
            self.from = None;
            self.target = Some(target);
            self.started = None;
            self.displayed = None;
            return;
        }
        if self.target == Some(target) {
            return;
        }
        let Some(from) = self.displayed else {
            self.target = Some(target);
            self.from = None;
            self.started = None;
            return;
        };
        if from == target {
            self.from = None;
            self.target = Some(target);
            self.started = None;
        } else {
            self.from = Some(from);
            self.target = Some(target);
            self.started = Some(now);
        }
    }

    fn render(&mut self, now: std::time::Instant, reduce_motion: bool) -> Option<KpiValues> {
        let target = self.target?;
        let Some(from) = self.from else {
            self.displayed = Some(target);
            return Some(target);
        };
        let Some(started) = self.started else {
            return Some(target);
        };
        if reduce_motion {
            self.from = None;
            self.started = None;
            self.displayed = Some(target);
            return Some(target);
        }
        let progress = now.saturating_duration_since(started).as_secs_f32()
            / KPI_ANIMATION_DURATION.as_secs_f32();
        if progress >= 1.0 {
            self.from = None;
            self.started = None;
            self.displayed = Some(target);
            return Some(target);
        }
        let value = from.interpolate(target, progress);
        self.displayed = Some(value);
        Some(value)
    }

    fn is_active(&self) -> bool {
        self.started.is_some()
    }

    fn hide(&mut self) {
        self.from = None;
        self.started = None;
        self.displayed = None;
    }
}

impl KpiValues {
    fn from_window(window: &UsageWindow) -> Self {
        Self {
            tokens: window.input_tokens + window.output_tokens,
            requests: window.requests,
            charged_microus: window.charged_microus,
            cache_rate: hit_rate(window),
            cost_usd: window.cost_usd,
            speed: speed_tokens_per_second(window),
        }
    }

    fn interpolate(self, target: Self, progress: f32) -> Self {
        fn optional(from: Option<f64>, to: Option<f64>, progress: f32) -> Option<f64> {
            match (from, to) {
                (Some(from), Some(to)) if from.is_finite() && to.is_finite() => {
                    Some(interpolated_value(from, to, progress))
                }
                (_, to) => to,
            }
        }
        Self {
            tokens: interpolated_integer_value(self.tokens, target.tokens, progress),
            requests: interpolated_integer_value(self.requests, target.requests, progress),
            charged_microus: interpolated_integer_value(
                self.charged_microus,
                target.charged_microus,
                progress,
            ),
            cache_rate: optional(self.cache_rate, target.cache_rate, progress),
            cost_usd: optional(self.cost_usd, target.cost_usd, progress),
            speed: optional(self.speed, target.speed, progress),
        }
    }
}

/// Aggregated trend, keyed by (generation, metric, split); hover frames
/// re-render with identical inputs and must not re-aggregate.
type TrendCache =
    std::cell::RefCell<Option<((u64, TrendMetric, TrendSplit), std::rc::Rc<TrendData>)>>;

/// The 客户端 filter over the overview's aggregates: every caller, one gateway
/// key that has no user, or every key one user owns. The wire spelling of a
/// selection is its [`UsageScope`]: `Key` sends `key_id` (labels are only unique
/// per user, so a key is named by id and displayed by its label), `User` sends
/// `user_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientFilter {
    All,
    Key { id: i64, label: String },
    User { id: i64, login: String },
}

impl ClientFilter {
    /// The usage scope this selection queries. The 动态 feed filters its rows
    /// by the same value, so the live rows and the aggregates above them
    /// always describe the same callers.
    pub(super) fn scope(&self) -> UsageScope {
        match self {
            Self::All => UsageScope::All,
            Self::Key { id, .. } => UsageScope::Key(*id),
            Self::User { id, .. } => UsageScope::User(*id),
        }
    }

    /// Dropdown display text of this selection (a user reads `用户 login`
    /// so it never looks like a key label of the same name).
    fn label(&self) -> String {
        match self {
            Self::All => "全部".to_string(),
            Self::Key { label, .. } => label.clone(),
            Self::User { login, .. } => format!("用户 {login}"),
        }
    }
}

/// The 客户端 dropdown's key entries: (id, label) of the built-in `owner`
/// user's keys only. Owner keys have no user identity, so they are the keys the
/// filter can offer one by one; every other key is reached through its user.
fn owner_key_options(keys: &[GatewayKeyRow]) -> Vec<(i64, String)> {
    keys.iter()
        .filter(|key| key.login == OWNER_LOGIN)
        .map(|key| (key.id, key.label.clone()))
        .collect()
}

/// The 客户端 dropdown's 按用户 group: (id, login) of the real users. The
/// built-in `owner` is left out — its keys appear as bare labels above instead.
fn real_users(users: &[UserRow]) -> Vec<(i64, String)> {
    users
        .iter()
        .filter(|user| user.login != OWNER_LOGIN)
        .map(|user| (user.id, user.login.clone()))
        .collect()
}

/// Usage statistics dashboard.
pub struct UsagePage {
    /// (whole-window aggregates, trend bucket rows, the window they were
    /// queried over — the chart grid must match the SQL's labels).
    state: LoadState<(UsageWindow, Vec<TrendRow>, ResolvedRange)>,
    /// Live-activity feed: the first Persisted event uses a short debounce;
    /// changes during a load retry 15 seconds after that load completes.
    feed: gpui::Entity<crate::activity::ActivityFeed>,
    active: bool,
    usage_reload: UsageReloadState,
    /// The currently running aggregate load. Dropping it aborts the HTTP task.
    usage_load_task: Option<gpui::Task<()>>,
    /// The pending persisted/trailing timer. Dropping it cancels the timer.
    usage_timer_task: Option<gpui::Task<()>>,
    revision_observer: LedgerRevisionObserver,
    /// Whether the overview's single elapsed-label repaint timer is in flight.
    elapsed_armed: bool,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    /// Bumped when a load's result lands in `state` — the trend-cache key must
    /// track data arrival, not request start (hover frames render mid-flight
    /// and would otherwise cache old rows under the new generation).
    pub(super) data_version: u64,
    /// Page-owned KPI clock and the last rendered values.
    kpi_animation: KpiAnimation,
    kpi_timer_armed: bool,
    /// Active window (a chip preset or the picker's custom range).
    pub(super) range: UsageRange,
    /// The custom-range date picker in the tab row.
    range_picker: gpui::Entity<DatePickerState>,
    /// Trend metric (Token / 金额 / 营收 / 请求, minus 营收 when 销售模式 is off);
    /// switching is client-side.
    pub(super) trend_metric: TrendMetric,
    /// Trend split dimension; switching refetches.
    pub(super) trend_split: TrendSplit,
    /// 客户端 dropdown's selection (全部 / one unowned key / one user);
    /// switching refetches.
    pub(super) client_filter: ClientFilter,
    /// Dropdown key entries: the built-in `owner`'s gateway keys as (id, label)
    /// — seeded from the pushed bootstrap document, refetched on a `resync` gap.
    /// A failure degrades to just the fixed 全部 entry.
    keys: Vec<(i64, String)>,
    /// Dropdown 按用户 group: the real users' (id, login), loaded from
    /// `/api/users` (not in the bootstrap document) with the built-in `owner`
    /// filtered out. A failure degrades to an empty group.
    users: Vec<(i64, String)>,
    /// Last seen activity-feed revisions: `bootstrap_rev` seeds the key list
    /// from the pushed document; `resync` (a stream-continuity gap) falls back
    /// to the pull reloads; `live_revision` is the render-only lifecycle signal.
    seen_bootstrap: u64,
    seen_live_revision: u64,
    /// Whether the 客户端 dropdown is open.
    client_list_open: bool,
    /// Bucket under the cursor (the hot band + tooltip).
    pub(super) trend_hover: Option<usize>,
    /// Ranked category the cursor is over (dims the other columns).
    pub(super) trend_emphasize: Option<String>,
    /// Plot geometry from the last chart prepaint, for hit-testing.
    pub(super) trend_geom: TrendGeometryCell,
    /// Aggregated trend, keyed by (generation, metric, split); hover frames
    /// re-render with identical inputs and must not re-aggregate.
    pub(super) trend_cache: TrendCache,
}

impl UsagePage {
    /// Screenshot-only readiness probe; absent from production builds.
    #[cfg(feature = "screenshot-test")]
    pub(crate) fn is_loaded_for_screenshot(&self) -> bool {
        matches!(self.state, LoadState::Loaded(_))
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if !active {
            self.cancel_usage_timer();
            self.usage_load_task = None;
            self.usage_reload
                .reduce(false, UsageReloadEvent::Deactivate);
        }
        if active {
            self.arm_elapsed(cx);
        } else {
            self.kpi_animation.hide();
        }
        cx.notify();
    }

    /// Hand one Usage reload event to the coordinator and perform the requested
    /// HTTP work. Persisted, manual, and resync events use a short debounce; a
    /// dirty load retries 15 seconds after that load finishes.
    fn apply_reload(&mut self, event: UsageReloadEvent, cx: &mut Context<Self>) {
        self.apply_reload_with_notice(event, None, cx);
    }

    /// Same reload decision as [`Self::apply_reload`], but carries the window
    /// of an explicit refresh through the debounce so that load reports one
    /// toast. Filter/range changes and feed events stay silent.
    fn apply_reload_with_notice(
        &mut self,
        event: UsageReloadEvent,
        handle: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        match self.usage_reload.reduce(self.active, event) {
            UsageReloadAction::None => {}
            UsageReloadAction::ReloadNow => self.reload_with_notice(cx, handle),
            UsageReloadAction::Schedule { token, delay } => {
                self.schedule_usage_timer(token, delay, handle, cx);
            }
        }
    }

    fn schedule_usage_timer(
        &mut self,
        token: u64,
        delay: std::time::Duration,
        handle: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        self.usage_timer_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |page, cx| {
                let current = page.usage_reload.timer_matches(token);
                if matches!(
                    page.usage_reload
                        .reduce(page.active, UsageReloadEvent::TimerComplete(token)),
                    UsageReloadAction::ReloadNow
                ) {
                    page.usage_timer_task = None;
                    page.reload_with_notice(cx, handle);
                } else if current {
                    page.usage_timer_task = None;
                }
            })
            .ok();
        }));
    }

    fn cancel_usage_timer(&mut self) {
        self.usage_timer_task = None;
        self.usage_reload.invalidate_timer();
    }

    fn finish_usage_load(&mut self, cx: &mut Context<Self>) {
        self.usage_load_task = None;
        if let UsageReloadAction::Schedule { token, delay } = self
            .usage_reload
            .reduce(self.active, UsageReloadEvent::LoadFinished)
        {
            self.schedule_usage_timer(token, delay, None, cx);
        }
    }

    /// Arm the one 500 ms elapsed-label repaint timer for the 动态 feed. It is
    /// self-extending while this page stays active and the feed still has a
    /// live attempt under the current 客户端 filter; otherwise it clears the
    /// arm and exits. One timer per live stretch — never one task per event,
    /// and it only notifies the page (no HTTP, no snapshot copies).
    fn arm_elapsed(&mut self, cx: &mut Context<Self>) {
        let has_live =
            crate::pages::feed::has_live_rows(self.feed.read(cx), self.client_filter.scope());
        if !should_arm_elapsed(self.active, self.elapsed_armed, has_live) {
            return;
        }
        self.elapsed_armed = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(ELAPSED_TICK).await;
                let keep = this
                    .update(cx, |page, cx| {
                        if !page.active {
                            page.elapsed_armed = false;
                            return false;
                        }
                        let has_live = crate::pages::feed::has_live_rows(
                            page.feed.read(cx),
                            page.client_filter.scope(),
                        );
                        if !has_live {
                            page.elapsed_armed = false;
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        })
        .detach();
    }

    fn arm_kpi_timer(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.kpi_timer_armed || !self.kpi_animation.is_active() {
            return;
        }
        self.kpi_timer_armed = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let keep = this
                    .update(cx, |page, cx| {
                        if !page.active || cx.reduce_motion() || !page.kpi_animation.is_active() {
                            if cx.reduce_motion() {
                                page.kpi_animation.hide();
                                cx.notify();
                            }
                            page.kpi_timer_armed = false;
                            return false;
                        }
                        if page
                            .kpi_animation
                            .started
                            .is_some_and(|start| start.elapsed() >= KPI_ANIMATION_DURATION)
                        {
                            page.kpi_timer_armed = false;
                            cx.notify();
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        })
        .detach();
    }
    pub(crate) fn new(
        feed: gpui::Entity<crate::activity::ActivityFeed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let range_picker =
            cx.new(|cx| crate::time::monday_picker(DatePickerState::range(window, cx)));
        // A custom range counts once both dates are picked; half-picked edits and
        // the chip-driven clear (Range(None, None)) don't move the window.
        cx.subscribe(
            &range_picker,
            |page, _picker, event: &DatePickerEvent, cx| {
                let DatePickerEvent::Change(PickerValue::Range(Some(start), Some(end))) = event
                else {
                    return;
                };
                let (Some(start), Some(end)) = (
                    crate::time::picker_date(PickerDate::Single(Some(start.date()))),
                    crate::time::picker_date(PickerDate::Single(Some(end.date()))),
                ) else {
                    return;
                };
                let range = UsageRange::Custom(start, end);
                if page.range != range {
                    page.range = range;
                    page.apply_reload(UsageReloadEvent::Manual, cx);
                }
            },
        )
        .detach();
        let mut this = Self {
            state: LoadState::Loading,
            active: false,
            usage_reload: UsageReloadState::default(),
            usage_load_task: None,
            usage_timer_task: None,
            revision_observer: LedgerRevisionObserver::new(
                feed.read(cx).resync(),
                feed.read(cx).persisted(),
            ),
            elapsed_armed: false,
            generation: 0,
            data_version: 0,
            kpi_animation: KpiAnimation::default(),
            kpi_timer_armed: false,
            range: UsageRange::Today,
            range_picker,
            trend_metric: TrendMetric::Tokens,
            trend_split: TrendSplit::Route,
            client_filter: ClientFilter::All,
            keys: Vec::new(),
            users: Vec::new(),
            seen_bootstrap: feed.read(cx).bootstrap_rev(),
            seen_live_revision: feed.read(cx).live_revision(),
            client_list_open: false,
            trend_hover: None,
            trend_emphasize: None,
            trend_geom: TrendGeometryCell::default(),
            trend_cache: TrendCache::new(None),
            feed,
        };
        // The first Persisted event uses a short debounce; changes during a load
        // retry 15 seconds after that load completes. Other events still repaint
        // the live block.
        cx.observe(&this.feed.clone(), |page, feed, cx| {
            // Seed the dropdown from the pushed document. The Status container
            // owns the initial aggregate load; bootstrap only refreshes labels.
            let bootstrap_rev = feed.read(cx).bootstrap_rev();
            if bootstrap_rev != page.seen_bootstrap {
                page.seen_bootstrap = bootstrap_rev;
                if let Some(document) = feed.read(cx).bootstrap() {
                    page.keys = owner_key_options(&document.gateway_keys);
                }
                if page.active {
                    page.reload_users(cx);
                }
            }
            let resync = feed.read(cx).resync();
            let persisted = feed.read(cx).persisted();
            match page.revision_observer.observe(resync, persisted) {
                LedgerRevisionChange::Resync => {
                    // The resync read is authoritative for the persisted
                    // revision observed in this pass; a later pass with a
                    // newer revision can still arm the debounce.
                    if page.active {
                        page.reload_keys(cx);
                        page.reload_users(cx);
                    }
                    page.apply_reload(UsageReloadEvent::Resync, cx);
                }
                LedgerRevisionChange::Persisted(_) => {
                    page.apply_reload(UsageReloadEvent::Persisted, cx);
                }
                LedgerRevisionChange::None => {}
            }
            page.seen_live_revision = feed.read(cx).live_revision();
            page.arm_elapsed(cx);
            cx.notify();
        })
        .detach();
        // A bootstrap that landed before this page existed still applies (same
        // seeding as the observer's bootstrap branch, but this page's first
        // connect arrives later, so the observer is the live path).
        let landed = this.feed.read(cx).bootstrap_rev();
        if this.seen_bootstrap == landed
            && landed > 0
            && let Some(document) = this.feed.read(cx).bootstrap()
        {
            this.keys = owner_key_options(&document.gateway_keys);
        }
        this
    }

    /// (Re)load the 客户端 dropdown's key list: the built-in `owner`'s keys,
    /// enabled or not, by label. A failure degrades silently to the fixed 全部
    /// entry; a reload is harmless to re-run.
    fn reload_keys(&mut self, cx: &mut Context<Self>) {
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.gateway_keys(None).await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if let Ok(keys) = result {
                    page.keys = owner_key_options(&keys);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// (Re)load the 客户端 dropdown's 按用户 group. A failure degrades
    /// silently to an empty group; a reload is harmless to re-run.
    fn reload_users(&mut self, cx: &mut Context<Self>) {
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move { admin.users().await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if let Ok(users) = result {
                    page.users = real_users(&users);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// (Re)load the active window of usage over the HTTP admin API. The
    /// range resolves at call time, so a preset's `now` stays live across
    /// feed-driven reloads. This is the direct immediate entry point for
    /// startup and trend changes; filter/range/manual events go through the
    /// short debounce in [`Self::apply_reload`].
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_with_notice(cx, None);
    }

    /// Same load as [`Self::reload`], but toasts the result of an explicit
    /// refresh. Internal reloads pass `None` and stay silent.
    fn reload_with_notice(&mut self, cx: &mut Context<Self>, handle: Option<AnyWindowHandle>) {
        self.cancel_usage_timer();
        self.usage_load_task = None;
        let _ = self
            .usage_reload
            .reduce(self.active, UsageReloadEvent::LoadStarted);
        crate::pages::begin_load(&mut self.state);
        self.generation += 1;
        let generation = self.generation;
        let resolved = self.range.resolve();
        let split = self.trend_split;
        let filter = self.client_filter.clone();
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call_cancellable(move |admin| async move {
                let window = admin
                    .usage_window(resolved.since, resolved.until, filter.scope())
                    .await?;
                let trend = admin
                    .usage_trend(
                        resolved.since,
                        resolved.until,
                        split,
                        resolved.offset,
                        filter.scope(),
                    )
                    .await?;
                Ok((window, trend, resolved))
            });
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    // The old task is normally already aborted. If its result was
                    // queued, it must not finish or mutate the new coordinator.
                    return;
                }
                match result {
                    Ok(rows) => {
                        page.kpi_animation.update(
                            (rows.0.requests != 0).then(|| KpiValues::from_window(&rows.0)),
                            std::time::Instant::now(),
                            page.active,
                            cx.reduce_motion(),
                        );
                        page.state = LoadState::Loaded(rows);
                        if let Some(handle) = handle {
                            notify_success(handle, cx, "刷新成功");
                        }
                    }
                    Err(error) => {
                        page.kpi_animation
                            .update(None, std::time::Instant::now(), false, false);
                        if let Some(handle) = handle {
                            notify_error(handle, cx, format!("刷新失败：{error}"));
                        }
                        page.state = LoadState::Failed(error);
                    }
                };
                page.arm_kpi_timer(cx);
                page.data_version += 1;
                page.finish_usage_load(cx);
                cx.notify();
            })
            .ok();
        });
        self.usage_load_task = Some(task);
    }

    /// The 客户端 filter dropdown: the trigger shows the current selection;
    /// the opened list floats above page content via `deferred` (never
    /// stretches the tab row). Choosing one closes the list and reloads.
    fn client_dropdown(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let trigger = Button::new("usage-client-filter")
            .label(self.client_filter.label())
            .outline()
            .compact()
            .on_click(cx.listener(|this, _, _, cx| {
                this.client_list_open = !this.client_list_open;
                cx.notify();
            }));
        if !self.client_list_open {
            return trigger.into_any_element();
        }
        let mut options = gpui::div()
            .id("usage-client-list")
            .flex()
            .flex_col()
            .border_1()
            .rounded_md()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .max_h(px(240.))
            .overflow_y_scroll()
            .child(self.client_option("usage-client-all", ClientFilter::All, cx));
        for (key_id, key_label) in &self.keys {
            let id = ElementId::from(gpui::SharedString::from(format!(
                "usage-client-key-{key_id}"
            )));
            let option = ClientFilter::Key {
                id: *key_id,
                label: key_label.clone(),
            };
            options = options.child(self.client_option(id, option, cx));
        }
        if !self.users.is_empty() {
            options = options.child(
                gpui::div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child("按用户"),
            );
        }
        for (id, login) in &self.users {
            let element_id =
                ElementId::from(gpui::SharedString::from(format!("usage-client-user-{id}")));
            let option = ClientFilter::User {
                id: *id,
                login: login.clone(),
            };
            options = options.child(self.client_option(element_id, option, cx));
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

    /// One clickable line in the 客户端 dropdown.
    fn client_option(
        &self,
        id: impl Into<ElementId>,
        option: ClientFilter,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let selected = self.client_filter == option;
        let text = option.label();
        gpui::div()
            .id(id)
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.client_list_open = false;
                if this.client_filter != option {
                    this.client_filter = option.clone();
                    this.apply_reload(UsageReloadEvent::Manual, cx);
                    this.arm_elapsed(cx);
                    cx.notify();
                } else {
                    cx.notify();
                }
            }))
    }

    /// Tab-row actions (rendered by the Status container): the period chips,
    /// the custom-range date picker, then the 客户端 filter and refresh — the
    /// two outline-style controls side by side at the end.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut actions: Vec<gpui::AnyElement> = Vec::new();
        actions.extend(
            UsageRange::PRESETS
                .iter()
                .map(|&(range, label)| {
                    let button = Button::new(gpui::ElementId::Name(
                        format!("usage-period-{label}").into(),
                    ))
                    .label(label)
                    .compact()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.range != range {
                            this.range = range;
                            // Leaving Custom: the picker's displayed dates go too.
                            this.range_picker.update(cx, |picker, cx| {
                                picker.set_date(PickerDate::Range(None, None), window, cx)
                            });
                            this.apply_reload(UsageReloadEvent::Manual, cx);
                        }
                    }));
                    if range == self.range {
                        button.secondary()
                    } else {
                        button.ghost()
                    }
                    .into_any_element()
                })
                .collect::<Vec<_>>(),
        );
        actions.push(
            DatePicker::new(&self.range_picker)
                .placeholder("自定义")
                .number_of_months(2)
                .w(px(170.))
                .into_any_element(),
        );
        actions.push(self.client_dropdown(cx));
        actions.push(
            refresh_button(
                "usage-refresh",
                cx.listener(|this, _, window, cx| {
                    this.reload_keys(cx);
                    this.reload_users(cx);
                    this.apply_reload_with_notice(
                        UsageReloadEvent::Manual,
                        Some(window.window_handle()),
                        cx,
                    );
                }),
            )
            .into_any_element(),
        );
        actions
    }
}

impl Render for UsagePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No header: the tab bar names the view and the refresh action lives in
        // the tab row, so the content starts immediately.
        let frame = page_frame("", "", cx);
        // Per-state body; the 动态 feed is appended ONCE below for every branch —
        // live traffic must stay visible while the window still reports no metered
        // requests (the first request of a new day, an empty Custom range, or the
        // Failed branch while the event listener is fine).
        let body = match &self.state {
            LoadState::Loading => frame.child(loading_card(cx)),
            LoadState::Failed(error) => frame.child(error_card(error, cx)),
            LoadState::Loaded((window, _, _)) if window.requests == 0 => frame.child(empty_card(
                "暂无用量",
                format!("{}内没有计量的请求。", self.range.label()),
                cx,
            )),
            LoadState::Loaded((window, trend, resolved)) => {
                // Read once: the 营收 card follows it, and the trend card coerces
                // its metric under it.
                let sales = crate::sales_mode::enabled();
                let kpi = KpiValues::from_window(window);
                let visible = self
                    .kpi_animation
                    .render(std::time::Instant::now(), cx.reduce_motion());
                frame
                    .child(
                        gpui::div()
                            .flex()
                            .gap_4()
                            .child(kpi_card(
                                "Token",
                                animated_integer_value(
                                    "tokens",
                                    visible.map(|values| values.tokens),
                                    kpi.tokens,
                                    humanize_count_overview,
                                    humanize_count_overview(kpi.tokens),
                                    cx,
                                ),
                                token_sub(window),
                                cx,
                            ))
                            .child(kpi_card(
                                "请求",
                                animated_integer_value(
                                    "requests",
                                    visible.map(|values| values.requests),
                                    kpi.requests,
                                    |value| value.to_string(),
                                    kpi.requests.to_string(),
                                    cx,
                                ),
                                requests_sub(window),
                                cx,
                            ))
                            // 营收 is a sales-mode reading: a deployment nobody is billed
                            // through has none, so the card is absent rather than zeroed.
                            .when(sales, |this| {
                                this.child(kpi_card(
                                    "营收",
                                    animated_integer_value(
                                        "charged",
                                        visible.map(|values| values.charged_microus),
                                        kpi.charged_microus,
                                        usd_microus,
                                        usd_microus(kpi.charged_microus),
                                        cx,
                                    ),
                                    "扣费合计".to_string(),
                                    cx,
                                ))
                            })
                            .child(hit_rate_card(visible, kpi, cx))
                            .child(kpi_card(
                                "金额",
                                animated_value(
                                    "cost",
                                    visible.and_then(|values| values.cost_usd),
                                    kpi.cost_usd,
                                    |value| format!("${value:.2}"),
                                    cost_value(window),
                                    cx,
                                ),
                                cost_sub(window),
                                cx,
                            ))
                            .child(kpi_card(
                                "输出速度",
                                animated_value(
                                    "speed",
                                    visible.and_then(|values| values.speed),
                                    kpi.speed,
                                    |value| format!("{} token/秒", value.round() as i64),
                                    speed_value(window),
                                    cx,
                                ),
                                speed_sub(window),
                                cx,
                            )),
                    )
                    .child(self.trend_card(trend, *resolved, cx))
            }
        };

        // The feed card is always the last child. Its one page-owned 500 ms
        // repaint timer keeps live elapsed labels current while the overview is
        // active; it reads the SAME scope the aggregates were queried with, so
        // selecting a client narrows the live rows with the numbers above them.
        body.child(crate::pages::feed::feed_card(
            &self.feed,
            self.client_filter.scope(),
            cx,
        ))
    }
}

/// One KPI card: small muted label, big value,
/// small muted sub line.
fn kpi_card(
    label: &'static str,
    value: gpui::AnyElement,
    sub: String,
    cx: &gpui::App,
) -> gpui::Div {
    card(cx)
        .flex_1()
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(value)
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(sub),
        )
}

/// Select the rate displayed for this frame, falling back to the current snapshot.
fn visible_cache_rate(visible: Option<f64>, current: Option<f64>) -> Option<f64> {
    match visible.zip(current) {
        Some((visible, current)) if visible.is_finite() && current.is_finite() => Some(visible),
        _ => current,
    }
}

/// The cache hit rate card has a meter under the value instead of a sub line.
fn hit_rate_card(visible: Option<KpiValues>, current: KpiValues, cx: &gpui::App) -> gpui::Div {
    let rate = visible_cache_rate(
        visible.and_then(|values| values.cache_rate),
        current.cache_rate,
    );
    let pct = rate.unwrap_or(0.) as f32;
    card(cx)
        .flex_1()
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child("缓存命中率"),
        )
        .child(kpi_value(
            rate.map(|rate| format!("{:.1}%", rate * 100.))
                .unwrap_or_else(|| "—".to_string()),
            cx,
        ))
        .child(
            gpui::div()
                .h(px(4.))
                .rounded_full()
                .bg(cx.theme().muted)
                .child(
                    gpui::div()
                        .h_full()
                        .rounded_full()
                        .bg(cx.theme().chart_2)
                        .w(relative(pct)),
                ),
        )
}

/// Render a KPI value, animating only when both snapshots contain a changed
/// numeric value. The final string remains authoritative for unavailable or
/// non-numeric values such as `—`.
fn animated_value(
    _name: &'static str,
    from: Option<f64>,
    to: Option<f64>,
    format: impl Fn(f64) -> String + 'static,
    final_value: String,
    cx: &gpui::App,
) -> gpui::AnyElement {
    let value = match from.zip(to) {
        Some((from, to)) if from.is_finite() && to.is_finite() => format(from),
        _ => final_value,
    };
    kpi_value(value, cx)
}

fn animated_integer_value(
    _name: &'static str,
    from: Option<i64>,
    to: i64,
    format: impl Fn(i64) -> String + 'static,
    final_value: String,
    cx: &gpui::App,
) -> gpui::AnyElement {
    let _ = to;
    kpi_value(from.map(format).unwrap_or(final_value), cx)
}

fn kpi_value(value: String, cx: &gpui::App) -> gpui::AnyElement {
    gpui::div()
        .text_size(px(22.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().foreground)
        .child(value)
        .into_any_element()
}

/// Sub line for the Token card: the breakdown. Cached is already contained
/// in input (usage contract), so the card total is input + output only.
fn token_sub(window: &UsageWindow) -> String {
    format!(
        "输入 {} · 输出 {} · 缓存 {}",
        humanize_count_overview(window.input_tokens),
        humanize_count_overview(window.output_tokens),
        humanize_count_overview(window.cached_input_tokens)
    )
}

/// Sub line for the Requests card: failures and the success share. The
/// ledger's unit is the attempt.
fn requests_sub(window: &UsageWindow) -> String {
    if window.failed == 0 {
        return "无失败".to_string();
    }
    let succeeded = 100. * (1. - window.failed as f64 / window.requests.max(1) as f64);
    format!("{} 次失败 · 成功 {:.1}%", window.failed, succeeded)
}

/// Cost card value: "$X.XX" over the priced rows, "—" when nothing was
/// priced.
fn cost_value(window: &UsageWindow) -> String {
    window
        .cost_usd
        .map(|cost| format!("${cost:.2}"))
        .unwrap_or_else(|| "—".to_string())
}

/// Cost card sub: unpriced rows are called out, otherwise the card reads as
/// priced at the effective amounts (there is no price list, only what was spent).
fn cost_sub(window: &UsageWindow) -> String {
    if window.unpriced > 0 {
        format!("{} 条未计价", window.unpriced)
    } else {
        "生效金额".to_string()
    }
}

/// Cache hit rate: the share of the prompt read from cache. `None` when the
/// window has no prompt tokens.
fn hit_rate(window: &UsageWindow) -> Option<f64> {
    (window.input_tokens > 0)
        .then(|| window.cached_input_tokens as f64 / window.input_tokens as f64)
}

/// Output speed as output tokens per second over successful rows'
/// decode windows (total_ms - ttft_ms).
fn speed_tokens_per_second(window: &UsageWindow) -> Option<f64> {
    (window.decode_ms > 0).then(|| window.timed_output as f64 / (window.decode_ms as f64 / 1000.))
}

/// Output speed display value.
fn speed_value(window: &UsageWindow) -> String {
    speed_tokens_per_second(window)
        .map(|speed| format!("{} token/秒", speed.round() as i64))
        .unwrap_or_else(|| "—".to_string())
}

/// Speed card sub: average whole-request duration over successful rows.
fn speed_sub(window: &UsageWindow) -> String {
    let ok = window.requests - window.failed;
    if window.timed_ms > 0 && ok > 0 {
        format!(
            "平均每请求 {:.1} 秒",
            window.timed_ms as f64 / ok as f64 / 1000.
        )
    } else {
        "无计时数据".to_string()
    }
}
