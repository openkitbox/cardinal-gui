//! Usage page: five KPI cards (led-strip blocks: label / big value /
//! sub line, or a meter for the cache hit rate) over the trend card, with the
//! 动态 feed (`pages::feed`) below. The led-trend (metric/split
//! switches, stacked-column chart with hover tooltip, per-category ranking)
//! lives in `usage_trend`. The cards read the whole-window aggregate
//! (`usage_window`); the trend reads bucket × category aggregates
//! (`usage_trend`).

use chrono::{DateTime, Duration, LocalResult, NaiveDate, TimeZone as _, Utc};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Div, ElementId, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, px, relative,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::calendar::Date as PickerDate;
use gpui_kit::component::date_picker::{
    DatePicker, DatePickerEvent, DatePickerState, DateTime as PickerValue,
};

use gateway::store::{GatewayKeyRow, TrendRow, TrendSplit, UsageWindow};

use crate::fmt::humanize_count;
use crate::pages::usage_trend::{TrendData, TrendGeometryCell, TrendMetric};
use crate::pages::{
    LoadState, card, empty_card, error_card, loading_card, page_frame, refresh_button,
};
use crate::runtime::GatewayRuntime;

/// The longest window the facade serves; [`UsageRange::resolve`] clamps to
/// the same span so the chart grid and the SQL cover identical instants.
const MAX_SPAN_DAYS: i64 = 370;

/// The window the page shows — the tab-row chips or the date picker's custom
/// range.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageRange {
    /// The local calendar day so far (midnight → now).
    Today,
    /// A rolling window of the last N days (`ts >= now() - N days`).
    Days(i32),
    /// A custom local-date range, both ends inclusive.
    Custom(NaiveDate, NaiveDate),
}

/// The query form of a [`UsageRange`]: `[since, until)` in UTC plus the local
/// offset the SQL and the bucket grid shift labels by — captured at resolve
/// so a load's query and chart agree even across an offset change.
#[derive(Clone, Copy)]
pub(crate) struct ResolvedRange {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub offset: Duration,
}

impl UsageRange {
    /// Chip row: preset ranges and their labels.
    const PRESETS: [(UsageRange, &'static str); 4] = [
        (UsageRange::Today, "今天"),
        (UsageRange::Days(7), "7 天"),
        (UsageRange::Days(30), "30 天"),
        (UsageRange::Days(90), "90 天"),
    ];

    fn resolve(self) -> ResolvedRange {
        let tz = crate::ui_tz::display_tz();
        let now = Utc::now();
        let offset = Duration::seconds(i64::from(tz.local_minus_utc()));
        let (since, until) = match self {
            Self::Today => (local_midnight(tz, now.with_timezone(&tz).date_naive()), now),
            Self::Days(days) => (now - Duration::days(i64::from(days)), now),
            // The picker speaks inclusive dates; the query is half-open.
            Self::Custom(start, end) => (
                local_midnight(tz, start),
                local_midnight(tz, end + Duration::days(1)),
            ),
        };
        ResolvedRange {
            since: since.clamp(until - Duration::days(MAX_SPAN_DAYS), until),
            until,
            offset,
        }
    }

    /// The empty-card period line ("今天内没有计量的请求。").
    fn label(self) -> String {
        match self {
            Self::Today => "今天".to_string(),
            Self::Days(days) => format!("{days} 天"),
            Self::Custom(start, end) => format!("{start} 至 {end}"),
        }
    }
}

/// The UTC instant of a display-day's 00:00. A fixed offset has no DST, so
/// the lookup is always unambiguous; the `LocalResult` arms stay for the
/// follow-the-system case where `display_tz` snapshots the local offset.
fn local_midnight(tz: chrono::FixedOffset, date: NaiveDate) -> DateTime<Utc> {
    let midnight = chrono::NaiveDateTime::new(date, chrono::NaiveTime::MIN);
    match tz.from_local_datetime(&midnight) {
        LocalResult::Single(local) => local.with_timezone(&Utc),
        LocalResult::Ambiguous(earliest, _) => earliest.with_timezone(&Utc),
        LocalResult::None => midnight.and_utc(),
    }
}

/// Aggregated trend, keyed by (generation, metric, split); hover frames
/// re-render with identical inputs and must not re-aggregate.
type TrendCache =
    std::cell::RefCell<Option<((u64, TrendMetric, TrendSplit), std::rc::Rc<TrendData>)>>;

/// The 客户端 filter over the overview's aggregates: every caller, one
/// caller identity (a gateway key label or a self-named `cardinal-x` value),
/// or the unidentified 本机 rows. The wire spelling of a selection: `Label`
/// sends the identity as-is, `Local` sends the reserved value `null`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ClientFilter {
    All,
    Local,
    Label(String),
}

impl ClientFilter {
    /// The `caller` query parameter value of this selection.
    fn query(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Local => Some("null"),
            Self::Label(label) => Some(label.as_str()),
        }
    }

    /// Dropdown display text of this selection.
    fn label(&self) -> &str {
        match self {
            Self::All => "全部",
            Self::Local => "本机",
            Self::Label(label) => label.as_str(),
        }
    }
}

/// Usage statistics dashboard.
pub struct UsagePage {
    /// (whole-window aggregates, trend bucket rows, the window they were
    /// queried over — the chart grid must match the SQL's labels).
    state: LoadState<(UsageWindow, Vec<TrendRow>, ResolvedRange)>,
    /// Live-activity feed: a Persisted event schedules a debounced reload.
    feed: gpui::Entity<crate::activity::ActivityFeed>,
    /// Last Persisted counter value we reacted to.
    seen_persisted: u64,
    /// A trailing-edge debounce timer is already armed.
    refresh_pending: bool,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    /// Bumped when a load's result lands in `state` — the trend-cache key must
    /// track data arrival, not request start (hover frames render mid-flight
    /// and would otherwise cache old rows under the new generation).
    pub(super) data_version: u64,
    /// Active window (a chip preset or the picker's custom range).
    pub(super) range: UsageRange,
    /// The custom-range date picker in the tab row.
    range_picker: gpui::Entity<DatePickerState>,
    /// Trend metric (Token / 费用 / 请求); switching is client-side.
    pub(super) trend_metric: TrendMetric,
    /// Trend split dimension; switching refetches.
    pub(super) trend_split: TrendSplit,
    /// 客户端 dropdown's selection (全部 / 本机 / one label); switching refetches.
    pub(super) client_filter: ClientFilter,
    /// Dropdown options: the registered gateway key labels — seeded from the
    /// pushed bootstrap document, refetched on a `resync` gap. A failure
    /// degrades to just the fixed 全部/本机 entries.
    key_labels: Vec<String>,
    /// Last seen activity-feed revisions: `bootstrap_rev` seeds the label list
    /// from the pushed document; `resync` (a stream-continuity gap) falls back
    /// to the pull reloads.
    seen_bootstrap: u64,
    seen_resync: u64,
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
    pub(crate) fn new(
        feed: gpui::Entity<crate::activity::ActivityFeed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let range_picker =
            cx.new(|cx| DatePickerState::range(window, cx).first_day_of_week(chrono::Weekday::Mon));
        // A custom range counts once both dates are picked; half-picked edits and
        // the chip-driven clear (Range(None, None)) don't move the window.
        cx.subscribe(
            &range_picker,
            |page, _picker, event: &DatePickerEvent, cx| {
                let DatePickerEvent::Change(PickerValue::Range(Some(start), Some(end))) = event
                else {
                    return;
                };
                let range = UsageRange::Custom(start.date(), end.date());
                if page.range != range {
                    page.range = range;
                    page.reload(cx);
                }
            },
        )
        .detach();
        let mut this = Self {
            state: LoadState::Loading,
            seen_persisted: feed.read(cx).persisted(),
            refresh_pending: false,
            generation: 0,
            data_version: 0,
            range: UsageRange::Today,
            range_picker,
            trend_metric: TrendMetric::Tokens,
            trend_split: TrendSplit::Route,
            client_filter: ClientFilter::All,
            key_labels: Vec::new(),
            seen_bootstrap: feed.read(cx).bootstrap_rev(),
            seen_resync: feed.read(cx).resync(),
            client_list_open: false,
            trend_hover: None,
            trend_emphasize: None,
            trend_geom: TrendGeometryCell::default(),
            trend_cache: TrendCache::new(None),
            feed,
        };
        // Read the connect revision off the page's own clone — `feed` moved in.
        // Persisted → aggregates changed: reload, debounced trailing-edge so a
        // settle storm coalesces into one query per second at most. Any feed
        // change also repaints: the 动态 card below the trend renders live rows,
        // whose phase/elapsed labels move without a persisted bump. The notify is
        // unconditional; the reload stays gated on the persisted counter.
        cx.observe(&this.feed.clone(), |page, feed, cx| {
            // A pushed bootstrap landed: the page's first connect — seed the
            // dropdown from it and open the aggregates (no eager load at
            // construction, so starting the client stays cheap).
            let bootstrap_rev = feed.read(cx).bootstrap_rev();
            if bootstrap_rev != page.seen_bootstrap {
                page.seen_bootstrap = bootstrap_rev;
                if let Some(document) = feed.read(cx).bootstrap() {
                    page.key_labels = document
                        .gateway_keys
                        .iter()
                        .map(|key| key.label.clone())
                        .collect();
                }
                page.reload(cx);
            }
            // A stream-continuity gap: the mirrors may be stale — fall back to
            // pulling (labels AND aggregates).
            let resync = feed.read(cx).resync();
            if resync != page.seen_resync {
                page.seen_resync = resync;
                page.reload_keys(cx);
                page.reload(cx);
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
                        page.reload(cx);
                    })
                    .ok();
                })
                .detach();
            }
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
            this.key_labels = document
                .gateway_keys
                .iter()
                .map(|key| key.label.clone())
                .collect();
            this.reload(cx);
        }
        this
    }

    /// (Re)load the 客户端 dropdown's label list (the registered gateway key
    /// labels, enabled or not). A failure degrades silently to the fixed
    /// 全部/本机 entries; a reload is harmless to re-run.
    fn reload_keys(&mut self, cx: &mut Context<Self>) {
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.gateway_keys().await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if let Ok(keys) = result {
                    page.key_labels = keys
                        .iter()
                        .map(|key: &GatewayKeyRow| key.label.clone())
                        .collect();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// (Re)load the active window of usage over the HTTP admin API. The
    /// range resolves at call time, so a preset's `now` stays live across
    /// feed-driven reloads.
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        crate::pages::begin_load(&mut self.state);
        self.generation += 1;
        let generation = self.generation;
        let resolved = self.range.resolve();
        let split = self.trend_split;
        let caller = self.client_filter.query().map(str::to_string);
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let window = admin
                    .usage_window(resolved.since, resolved.until, caller.as_deref())
                    .await?;
                let trend = admin
                    .usage_trend(
                        resolved.since,
                        resolved.until,
                        split,
                        resolved.offset,
                        caller.as_deref(),
                    )
                    .await?;
                Ok((window, trend, resolved))
            });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // a newer reload is in flight; this snapshot is stale
                }
                page.state = match result {
                    Ok(rows) => LoadState::Loaded(rows),
                    Err(error) => LoadState::Failed(error),
                };
                page.data_version += 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The 客户端 filter dropdown: the trigger shows the current selection;
    /// the opened list floats above page content via `deferred` (never
    /// stretches the tab row). Choosing one closes the list and reloads.
    fn client_dropdown(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selection = self.client_filter.clone();
        let trigger = Button::new("usage-client-filter")
            .label(selection.label().to_string())
            .outline()
            .compact()
            .on_click(cx.listener(|this, _, _, cx| {
                this.client_list_open = !this.client_list_open;
                cx.notify();
            }));
        if !self.client_list_open {
            return trigger.into_any_element();
        }
        let fixed = [
            ("usage-client-all", ClientFilter::All),
            ("usage-client-local", ClientFilter::Local),
        ];
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
            .overflow_y_scroll();
        for (id, option) in fixed {
            options = options.child(self.client_option(id, option, cx));
        }
        for (index, label) in self.key_labels.iter().enumerate() {
            let id = ElementId::from(gpui::SharedString::from(format!(
                "usage-client-label-{index}"
            )));
            options = options.child(self.client_option(id, ClientFilter::Label(label.clone()), cx));
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
        let text = option.label().to_string();
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
                    this.reload(cx);
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
                            this.reload(cx);
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
                .small()
                .number_of_months(2)
                .w(px(212.))
                .into_any_element(),
        );
        actions.push(self.client_dropdown(cx));
        actions.push(
            refresh_button(
                "usage-refresh",
                cx.listener(|this, _, _, cx| {
                    this.reload_keys(cx);
                    this.reload(cx);
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
            LoadState::Loaded((window, trend, resolved)) => frame
                .child(
                    gpui::div()
                        .flex()
                        .gap_4()
                        .child(kpi_card(
                            "Token",
                            humanize_count(window.input_tokens + window.output_tokens),
                            token_sub(window),
                            cx,
                        ))
                        .child(kpi_card(
                            "请求",
                            window.requests.to_string(),
                            requests_sub(window),
                            cx,
                        ))
                        .child(kpi_card("费用", cost_value(window), cost_sub(window), cx))
                        .child(hit_rate_card(window, cx))
                        .child(kpi_card(
                            "输出速度",
                            speed_value(window),
                            speed_sub(window),
                            cx,
                        )),
                )
                .child(self.trend_card(trend, *resolved, cx)),
        };

        // The feed card is always the last child. This page has no wire-flow
        // ticker, and the feed adds none: live rows render steady, and no repaint
        // happens for animation's sake (the dots carry the client's colour).
        body.child(crate::pages::feed::feed_card(&self.feed, cx))
    }
}

/// One KPI card: small muted label, big value,
/// small muted sub line.
fn kpi_card(label: &'static str, value: String, sub: String, cx: &gpui::App) -> gpui::Div {
    card(cx)
        .flex_1()
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(
            gpui::div()
                .text_size(px(22.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(value),
        )
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(sub),
        )
}

/// The cache hit rate card: same block shape, but a meter under the value
/// instead of a sub line.
fn hit_rate_card(window: &UsageWindow, cx: &gpui::App) -> gpui::Div {
    let (value, pct) = match hit_rate(window) {
        Some(rate) => (format!("{:.1}%", rate * 100.), rate as f32),
        None => ("—".to_string(), 0.),
    };
    card(cx)
        .flex_1()
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child("缓存命中率"),
        )
        .child(
            gpui::div()
                .text_size(px(22.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(value),
        )
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

/// Sub line for the Token card: the breakdown. Cached is already contained
/// in input (usage contract), so the card total is input + output only.
fn token_sub(window: &UsageWindow) -> String {
    format!(
        "输入 {} · 输出 {} · 缓存 {}",
        humanize_count(window.input_tokens),
        humanize_count(window.output_tokens),
        humanize_count(window.cached_input_tokens)
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
/// priced at the effective prices.
fn cost_sub(window: &UsageWindow) -> String {
    if window.unpriced > 0 {
        format!("{} 条未计价", window.unpriced)
    } else {
        "生效价格".to_string()
    }
}

/// Cache hit rate: the share of the prompt read from cache. `None` when the
/// window has no prompt tokens.
fn hit_rate(window: &UsageWindow) -> Option<f64> {
    (window.input_tokens > 0)
        .then(|| window.cached_input_tokens as f64 / window.input_tokens as f64)
}

/// Output speed value: output tokens a second over successful rows'
/// decode windows (total_ms - ttft_ms).
fn speed_value(window: &UsageWindow) -> String {
    if window.decode_ms > 0 {
        format!(
            "{} token/秒",
            (window.timed_output as f64 / (window.decode_ms as f64 / 1000.)).round() as i64
        )
    } else {
        "—".to_string()
    }
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
