//! Usage trend card: a segmented metric switch (Token /
//! 费用 / 请求) and a split-dimension switch (路由 / 模型 / 供应商 / 客户端) over a
//! stacked-column chart with a hover tooltip, flanked by a per-category
//! ranking. The top 7 categories take the palette colours; the rest fold
//! into 其他. Geometry and paint happen in a `canvas` element; the tooltip
//! and the ranking are regular divs positioned from the geometry the last
//! prepaint stored.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gateway::store::{TrendRow, TrendSplit};
use gpui::{
    App, Bounds, Context, Corners, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, ShapedLine, SharedString, StatefulInteractiveElement, Styled, TextAlign,
    TextRun, Window, canvas, div, point, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::component::ActiveTheme;

use crate::fmt::humanize_count;
use crate::pages::card;
use crate::pages::usage::{ResolvedRange, UsagePage};

/// The light-mode chart palette; the GUI is light-only.
const PALETTE: [u32; 7] = [
    0x4f46e5, 0x0ea5e9, 0x10b981, 0xf59e0b, 0xec4899, 0x8b5cf6, 0x64748b,
];
/// The faint color: the folded 其他 category and the hover band use it.
const FAINT: u32 = 0xb9b9c2;
/// Categories told apart in the chart; the rest fold into 其他.
const TOP_CATEGORIES: usize = 7;
const PLOT_HEIGHT: f32 = 236.;
const MARGIN_TOP: f32 = 8.;
const MARGIN_BOTTOM: f32 = 22.;
const MARGIN_RIGHT: f32 = 6.;
const LABEL_GAP: f32 = 6.;
const AXIS_FONT: f32 = 10.5;
const AXIS_LINE_HEIGHT: f32 = 14.;
/// Bar width is 68% of the slot, clamped (`max(2, min(30, slot * 0.68))`).
const MAX_BAR_WIDTH: f32 = 30.;
/// X ticks are thinned to one per ~62px of plot width.
const TICK_SPACING: f32 = 62.;
const RANK_WIDTH: f32 = 272.;

fn palette(index: usize) -> Hsla {
    gpui::rgb(PALETTE[index]).into()
}

/// Trend metric of the chart and the ranking (speed is window-level only, so
/// the trend offers the three additive metrics).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrendMetric {
    Tokens,
    Cost,
    Requests,
}

impl TrendMetric {
    pub(crate) const ALL: [TrendMetric; 3] = [Self::Tokens, Self::Cost, Self::Requests];

    fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Token",
            Self::Cost => "费用",
            Self::Requests => "请求",
        }
    }

    fn value(self, row: &TrendRow) -> f64 {
        match self {
            // cached_input_tokens is already contained in input_tokens
            // (llm::server::usage contract) — same sum as the KPI card.
            Self::Tokens => (row.input_tokens + row.output_tokens) as f64,
            Self::Cost => row.cost_usd.unwrap_or(0.),
            Self::Requests => row.requests as f64,
        }
    }

    /// Short axis/rank format.
    fn format(self, value: f64) -> String {
        match self {
            Self::Tokens => humanize_count(value.round() as i64),
            Self::Requests => format!("{}", value.round() as i64),
            // No more digits than it takes.
            Self::Cost => {
                if value >= 10. {
                    format!("${value:.0}")
                } else if value >= 1. {
                    format!("${value:.1}")
                } else {
                    format!("${value:.2}")
                }
            }
        }
    }

    /// Long tooltip format.
    fn format_long(self, value: f64) -> String {
        match self {
            Self::Cost => format!("${value:.2}"),
            _ => self.format(value),
        }
    }
}

/// Fallback display name of a NULL category (alias NULL = 直连).
fn category_fallback(split: TrendSplit) -> &'static str {
    match split {
        TrendSplit::Route => "（直连）",
        TrendSplit::Model | TrendSplit::Provider => "（未知）",
        // No agent and no caller key: a local unidentified caller.
        TrendSplit::Client => "（本机）",
    }
}

/// One ranked category (a top one with a palette colour, or the folded 其他).
pub(crate) struct TrendCategory {
    key: String,
    color: Hsla,
    requests: i64,
    failed: i64,
    tokens: i64,
    cost: Option<f64>,
    /// Successful-row timing (speed line under the rank bar): output tokens
    /// over decode windows (total_ms - ttft_ms).
    timed_output: i64,
    decode_ms: i64,
    metric_value: f64,
}

/// One chart column: the bucket's metric total and each top category's part.
struct TrendBucket {
    /// Raw bucket string (`YYYY-MM-DD` / `YYYY-MM-DD HH:00`), the tooltip title.
    label: String,
    /// Axis tick (`MM-DD` / `HH:00`).
    tick: String,
    total: f64,
    requests: i64,
    /// Metric value per top category, plus the folded 其他 as the last entry.
    parts: Vec<f64>,
}

pub(crate) struct TrendData {
    categories: Vec<TrendCategory>,
    other: Option<TrendCategory>,
    buckets: Vec<TrendBucket>,
    metric_total: f64,
}

/// Aggregate rows into ranked categories and per-bucket stacks over a
/// continuous time axis: the buckets are
/// the window's full hour/day grid, rows land by label.
fn prepare(
    rows: &[TrendRow],
    metric: TrendMetric,
    split: TrendSplit,
    range: ResolvedRange,
) -> TrendData {
    #[derive(Clone, Copy, Default)]
    struct Agg {
        requests: i64,
        failed: i64,
        tokens: i64,
        cost: f64,
        priced: bool,
        timed_output: i64,
        decode_ms: i64,
        metric: f64,
    }

    // Continuous time axis: every bucket of the window exists, rows land by
    // label (a row outside the grid — clock skew between query and render —
    // gets appended at the end rather than dropped).
    let mut bucket_labels = bucket_grid(range);
    let mut bucket_index: HashMap<String, usize> = bucket_labels
        .iter()
        .enumerate()
        .map(|(i, label)| (label.clone(), i))
        .collect();
    let mut bucket_maps: Vec<HashMap<String, f64>> =
        (0..bucket_labels.len()).map(|_| HashMap::new()).collect();
    let mut bucket_requests: Vec<i64> = vec![0; bucket_labels.len()];
    let mut aggs: HashMap<String, Agg> = HashMap::new();
    for row in rows {
        let category = row
            .category
            .clone()
            .unwrap_or_else(|| category_fallback(split).to_string());
        let bucket = match bucket_index.get(&row.bucket) {
            Some(&index) => index,
            None => {
                bucket_index.insert(row.bucket.clone(), bucket_labels.len());
                bucket_labels.push(row.bucket.clone());
                bucket_maps.push(HashMap::new());
                bucket_requests.push(0);
                bucket_labels.len() - 1
            }
        };
        let agg = aggs.entry(category.clone()).or_default();
        agg.requests += row.requests;
        agg.failed += row.failed;
        agg.tokens += row.input_tokens + row.output_tokens;
        if let Some(cost) = row.cost_usd {
            agg.cost += cost;
            agg.priced = true;
        }
        agg.timed_output += row.timed_output;
        agg.decode_ms += row.decode_ms;
        let value = metric.value(row);
        agg.metric += value;
        *bucket_maps[bucket].entry(category).or_default() += value;
        bucket_requests[bucket] += row.requests;
    }

    let mut ranked: Vec<(String, Agg)> = aggs.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.metric
            .total_cmp(&a.1.metric)
            .then_with(|| b.1.tokens.cmp(&a.1.tokens))
    });

    let category_of = |(key, agg): (String, Agg), color: Hsla| TrendCategory {
        key,
        color,
        requests: agg.requests,
        failed: agg.failed,
        tokens: agg.tokens,
        cost: agg.priced.then_some(agg.cost),
        timed_output: agg.timed_output,
        decode_ms: agg.decode_ms,
        metric_value: agg.metric,
    };
    let (top, rest) = ranked.split_at(ranked.len().min(TOP_CATEGORIES));
    let categories: Vec<TrendCategory> = top
        .iter()
        .enumerate()
        .map(|(i, (key, agg))| category_of((key.clone(), *agg), palette(i)))
        .collect();
    let has_other = !rest.is_empty();
    let other = has_other.then(|| {
        let folded = rest.iter().fold(Agg::default(), |mut acc, (_, agg)| {
            acc.requests += agg.requests;
            acc.failed += agg.failed;
            acc.tokens += agg.tokens;
            acc.cost += agg.cost;
            acc.priced |= agg.priced;
            acc.timed_output += agg.timed_output;
            acc.decode_ms += agg.decode_ms;
            acc.metric += agg.metric;
            acc
        });
        category_of(
            (format!("其他（{}）", rest.len()), folded),
            gpui::rgb(FAINT).into(),
        )
    });

    let buckets: Vec<TrendBucket> = bucket_labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let map = &bucket_maps[i];
            let mut parts: Vec<f64> = categories
                .iter()
                .map(|c| map.get(&c.key).copied().unwrap_or(0.))
                .collect();
            let total: f64 = map.values().sum();
            // The folded 其他 segment exists only when there ARE folded categories;
            // otherwise total == stacked modulo summation order (f64 epsilon), which
            // must not surface as a phantom gray sliver.
            if has_other {
                let stacked: f64 = parts.iter().sum();
                parts.push((total - stacked).max(0.));
            }
            TrendBucket {
                tick: tick_label(label).to_string(),
                label: label.clone(),
                total,
                requests: bucket_requests[i],
                parts,
            }
        })
        .collect();

    TrendData {
        categories,
        other,
        buckets,
        metric_total: ranked.iter().map(|(_, agg)| agg.metric).sum(),
    }
}

/// The window's full bucket grid in local wall clock (the SQL shifts its
/// labels by the same offset, so rows land by label): hourly for a sub-day
/// window ([`gateway::store::hourly_buckets`]), else local calendar days from
/// the start's date to the date of the window's last instant (`until` is
/// exclusive). Local midnight/hour boundaries are 0 mod 86400/3600 in the
/// shifted clock, so plain epoch-stepping produces aligned labels.
fn bucket_grid(range: ResolvedRange) -> Vec<String> {
    let offset = range.offset.num_seconds();
    let shift = |t: chrono::DateTime<chrono::Utc>| t.timestamp() + offset;
    let label = |ts: i64, hourly: bool| {
        chrono::DateTime::from_timestamp(ts, 0).map(|t| {
            t.format(if hourly { "%Y-%m-%d %H:00" } else { "%Y-%m-%d" })
                .to_string()
        })
    };
    if gateway::store::hourly_buckets(range.since, range.until) {
        let start = shift(range.since) / 3600 * 3600;
        let end = (shift(range.until) - 1).max(start) / 3600 * 3600;
        return (start..=end)
            .step_by(3600)
            .filter_map(|ts| label(ts, true))
            .collect();
    }
    let start = shift(range.since) / 86400 * 86400;
    // The last day the window touches: `until` itself is excluded, so step
    // back a second (a degenerate empty window still yields its start day).
    let end = (shift(range.until) - 1).max(start) / 86400 * 86400;
    (start..=end)
        .step_by(86400)
        .filter_map(|ts| label(ts, false))
        .collect()
}

/// Axis tick of a bucket label: the date without the year, the hour alone.
fn tick_label(bucket: &str) -> &str {
    if bucket.len() == 10 {
        &bucket[5..]
    } else {
        bucket.get(11..).unwrap_or(bucket)
    }
}

/// A nice axis top with four steps under it.
fn axis_top(max: f64) -> f64 {
    if max <= 0. {
        return 4.;
    }
    let e = 10f64.powf((max / 4.).log10().floor());
    for m in [1., 2., 2.5, 5., 10.] {
        if m * e * 4. >= max {
            return m * e * 4.;
        }
    }
    10. * e * 4.
}

/// Plot geometry the last canvas prepaint computed; the mouse handlers and
/// the tooltip overlay read it (hit-testing and placement).
#[derive(Clone, Copy)]
pub(crate) struct TrendGeometry {
    origin: Point<Pixels>,
    margin_left: f32,
    plot_width: f32,
    slot: f32,
    count: usize,
}

impl TrendGeometry {
    /// The bucket under a window-space mouse position, if any.
    fn bucket_at(&self, position: Point<Pixels>) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let rel: f32 = (position.x - self.origin.x).into();
        let rel = rel - self.margin_left;
        if rel < 0. || rel > self.plot_width {
            return None;
        }
        Some(((rel / self.slot) as usize).min(self.count - 1))
    }
}

/// One stacked bar segment to paint.
struct BarSegment {
    bounds: Bounds<Pixels>,
    color: Hsla,
    /// Category key for hover emphasis; empty = the folded 其他.
    key: SharedString,
}

struct Prepainted {
    geometry: TrendGeometry,
    grid: Vec<f32>,
    y_labels: Vec<(ShapedLine, Point<Pixels>)>,
    x_ticks: Vec<(ShapedLine, Point<Pixels>)>,
    bars: Vec<BarSegment>,
    hot: Option<Bounds<Pixels>>,
}

/// Everything prepaint needs besides the layout bounds: the data, the
/// current view state, and the theme colours, resolved once in render.
struct ChartInput {
    data: Rc<TrendData>,
    metric: TrendMetric,
    hover: Option<usize>,
    axis_color: Hsla,
    geom_cell: TrendGeometryCell,
}

pub(crate) type TrendGeometryCell = Rc<RefCell<Option<TrendGeometry>>>;

impl UsagePage {
    /// The led-trend card: switches on top, chart left, ranking right.
    pub(crate) fn trend_card(
        &self,
        rows: &[TrendRow],
        range: ResolvedRange,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let metric = self.trend_metric;
        let split = self.trend_split;
        // Hover/emphasize frames re-render with identical inputs; only new rows
        // (data_version, bumped when a load lands) or a metric/split switch
        // re-aggregates.
        let cache_key = (self.data_version, metric, split);
        let data = {
            let mut cache = self.trend_cache.borrow_mut();
            match &*cache {
                Some((key, data)) if *key == cache_key => Rc::clone(data),
                _ => {
                    let data = Rc::new(prepare(rows, metric, split, range));
                    *cache = Some((cache_key, Rc::clone(&data)));
                    data
                }
            }
        };

        card(cx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(segs(
                        "trend-metric",
                        TrendMetric::ALL.iter().map(|m| (*m, m.label())),
                        metric,
                        cx,
                        |page, m, cx| {
                            page.trend_metric = m;
                            cx.notify();
                        },
                    ))
                    .child(div().flex_1())
                    .child(segs(
                        "trend-split",
                        [
                            (TrendSplit::Route, "路由"),
                            (TrendSplit::Model, "模型"),
                            (TrendSplit::Provider, "供应商"),
                            (TrendSplit::Client, "客户端"),
                        ]
                        .into_iter(),
                        split,
                        cx,
                        |page, split, cx| {
                            page.trend_split = split;
                            page.reload(cx);
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .mt_2()
                    .child(self.trend_chart(Rc::clone(&data), cx))
                    .child(self.trend_rank(Rc::clone(&data), cx)),
            )
    }

    /// The stacked-column chart: canvas-painted grid/axis/bars plus an
    /// absolutely positioned tooltip overlay.
    fn trend_chart(&self, data: Rc<TrendData>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_grid = cx.theme().border;
        let theme_hot: Hsla = gpui::rgb(0xf1f1f4).into();
        let metric = self.trend_metric;
        let hover = self.trend_hover;
        let emphasize = self.trend_emphasize.clone();

        let input = ChartInput {
            data: Rc::clone(&data),
            metric,
            hover,
            axis_color: cx.theme().muted_foreground,
            geom_cell: self.trend_geom.clone(),
        };
        let chart = canvas(
            move |bounds, window, _cx| prepaint_chart(bounds, &input, window),
            move |bounds, pre, window, cx| {
                paint_chart(
                    bounds,
                    pre,
                    theme_grid,
                    theme_hot,
                    emphasize.as_deref(),
                    window,
                    cx,
                )
            },
        );

        let mut area = div()
            .id("usage-trend-chart")
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(PLOT_HEIGHT))
            .on_mouse_move(cx.listener(|page, event: &gpui::MouseMoveEvent, _, cx| {
                let hover = page
                    .trend_geom
                    .borrow()
                    .and_then(|g| g.bucket_at(event.position));
                if page.trend_hover != hover {
                    page.trend_hover = hover;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|page, hovered, _, cx| {
                if !hovered && page.trend_hover.is_some() {
                    page.trend_hover = None;
                    cx.notify();
                }
            }))
            .child(chart.size_full());

        if let Some(bucket) = hover
            && let Some(geometry) = *self.trend_geom.borrow()
            && bucket < data.buckets.len()
        {
            area = area.child(tooltip(&data, metric, bucket, geometry, cx));
        }
        area
    }

    /// The per-category ranking beside the chart: swatch,
    /// name, value; a share bar; a stats line. Hovering a row dims the other
    /// categories' columns.
    fn trend_rank(&self, data: Rc<TrendData>, cx: &mut Context<Self>) -> impl IntoElement {
        let metric = self.trend_metric;
        let total = data.metric_total;
        let mut list = div()
            .id("usage-trend-rank")
            .w(px(RANK_WIDTH))
            .h(px(PLOT_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        let leader = data
            .categories
            .first()
            .map_or(1., |c| c.metric_value.max(1.));
        for category in &data.categories {
            list = list.child(self.rank_row(category, leader, total, metric, false, cx));
        }
        if let Some(other) = &data.other {
            list = list.child(self.rank_row(other, leader, total, metric, true, cx));
        }
        list
    }

    fn rank_row(
        &self,
        category: &TrendCategory,
        leader: f64,
        total: f64,
        metric: TrendMetric,
        plain: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let share = (100. * category.metric_value / leader).max(1.5) as f32;
        let key: SharedString = category.key.clone().into();
        let row = div()
            .id(ElementId::Name(
                format!("trend-rank-{}", category.key).into(),
            ))
            .w_full()
            .px_2()
            .py(px(6.))
            .rounded(px(8.))
            .flex()
            .flex_col()
            .when(!plain, |row| {
                row.cursor_pointer().hover(|s| s.bg(theme.muted))
            })
            .when(!plain, |row| {
                row.on_hover(cx.listener({
                    let key = key.clone();
                    move |page: &mut UsagePage, hovered: &bool, _, cx: &mut Context<UsagePage>| {
                        let emphasize = (*hovered).then(|| key.to_string());
                        if page.trend_emphasize != emphasize {
                            page.trend_emphasize = emphasize;
                            cx.notify();
                        }
                    }
                }))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .child(
                        div()
                            .size(px(8.))
                            .rounded(px(2.))
                            .flex_none()
                            .bg(category.color),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(category.key.clone()),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .pl_2()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .whitespace_nowrap()
                            .child(metric.format_long(category.metric_value)),
                    ),
            )
            .child(
                div()
                    .h(px(4.))
                    .mt_1()
                    .ml(px(15.))
                    .rounded(px(2.))
                    .bg(theme.muted)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded(px(2.))
                            .bg(category.color)
                            .w(relative(share / 100.)),
                    ),
            );

        // The rank row: every figure but the ranked metric, then the share.
        let mut stats: Vec<String> = Vec::new();
        if metric != TrendMetric::Requests {
            stats.push(format!("{} 请求", category.requests));
        }
        if metric != TrendMetric::Tokens {
            stats.push(format!("{} token", humanize_count(category.tokens)));
        }
        if metric != TrendMetric::Cost
            && let Some(cost) = category.cost
        {
            stats.push(format!("${cost:.2}"));
        }
        if category.decode_ms > 0 {
            stats.push(format!(
                "{:.1}t/s",
                category.timed_output as f64 * 1000. / category.decode_ms as f64
            ));
        }
        if total > 0. {
            stats.push(format!("{:.0}%", 100. * category.metric_value / total));
        }
        let mut line = div()
            .flex()
            .ml(px(15.))
            .mt_0p5()
            .text_size(px(10.5))
            .whitespace_nowrap()
            .text_color(theme.muted_foreground)
            .child(stats.join(" · "));
        if category.failed > 0 {
            line = line.child(
                div()
                    .text_color(theme.danger)
                    .child(format!(" · {} 次失败", category.failed)),
            );
        }
        row.child(line)
    }
}

/// Segmented control: a pill track, the active option on a
/// raised thumb.
fn segs<T: Copy + Eq + 'static>(
    id_prefix: &'static str,
    options: impl Iterator<Item = (T, &'static str)>,
    current: T,
    cx: &mut Context<UsagePage>,
    on_pick: impl Fn(&mut UsagePage, T, &mut Context<UsagePage>) + 'static + Copy,
) -> impl IntoElement {
    let theme = cx.theme();
    let mut track = div().flex().p(px(2.)).rounded(px(7.)).bg(theme.muted);
    for (i, (value, label)) in options.enumerate() {
        let on = value == current;
        let mut option = div()
            .id(ElementId::Name(format!("{id_prefix}-{i}").into()))
            .h(px(22.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(5.))
            .text_size(px(12.))
            .cursor_pointer()
            .text_color(if on {
                theme.foreground
            } else {
                theme.muted_foreground
            })
            .when(!on, |option| {
                option.hover(|s| s.text_color(theme.foreground))
            })
            .child(label);
        if on {
            option = option.bg(theme.popover).shadow_sm();
        }
        track = track
            .child(option.on_click(cx.listener(move |page, _, _, cx| on_pick(page, value, cx))));
    }
    track
}

/// Chart prepaint: compute geometry (stored for the mouse handlers), shape
/// the axis labels, lay out the stacked bars and the hover band.
fn prepaint_chart(bounds: Bounds<Pixels>, input: &ChartInput, window: &mut Window) -> Prepainted {
    let ChartInput {
        data,
        metric,
        hover,
        axis_color,
        geom_cell,
    } = input;
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let count = data.buckets.len();
    let top = axis_top(data.buckets.iter().map(|b| b.total).fold(0., f64::max));

    let font = window.text_style().font();
    let axis_color = *axis_color;
    let shape = |window: &mut Window, text: &str| {
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color: axis_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(
            SharedString::from(text.to_string()),
            px(AXIS_FONT),
            &[run],
            None,
        )
    };

    // Y labels: 5 steps from the nice top down to 0; the plot starts where the
    // widest label ends.
    let y_labels: Vec<(ShapedLine, f64)> = (0..=4)
        .map(|i| {
            (
                shape(window, &metric.format(top * (1. - i as f64 / 4.))),
                top * (1. - i as f64 / 4.),
            )
        })
        .collect();
    let widest: f32 = y_labels
        .iter()
        .map(|(line, _)| f32::from(line.width()))
        .fold(0., f32::max);
    let margin_left = widest + LABEL_GAP;

    let plot_width = width - margin_left - MARGIN_RIGHT;
    let plot_height = height - MARGIN_TOP - MARGIN_BOTTOM;
    // Clamp once: drawing and hit-testing share the same slot, even in the
    // degenerate plot_width <= 0 layout.
    let slot = (if count > 0 {
        plot_width / count as f32
    } else {
        plot_width
    })
    .max(1.);
    let bar_width = (slot * 0.68).clamp(2., MAX_BAR_WIDTH);
    let y_of = |v: f64| MARGIN_TOP + plot_height - (plot_height as f64 * v / top) as f32;

    let grid: Vec<f32> = (0..=4)
        .map(|i| MARGIN_TOP + plot_height * i as f32 / 4.)
        .collect();
    let y_labels: Vec<(ShapedLine, Point<Pixels>)> = y_labels
        .into_iter()
        .enumerate()
        .map(|(i, (line, _))| {
            let origin = bounds.origin
                + point(
                    px(margin_left - LABEL_GAP - f32::from(line.width())),
                    px(grid[i] - AXIS_LINE_HEIGHT / 2.),
                );
            (line, origin)
        })
        .collect();

    // X ticks, thinned so one label has ~62px.
    let room = (plot_width / TICK_SPACING).floor().max(2.) as usize;
    let every = count.div_ceil(room).max(1);
    let x_ticks: Vec<(ShapedLine, Point<Pixels>)> = data
        .buckets
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0)
        .map(|(i, bucket)| {
            let line = shape(window, &bucket.tick);
            let origin = bounds.origin
                + point(
                    px(margin_left + slot * (i as f32 + 0.5) - f32::from(line.width()) / 2.),
                    px(height - MARGIN_BOTTOM + 4.),
                );
            (line, origin)
        })
        .collect();

    let geometry = TrendGeometry {
        origin: bounds.origin,
        margin_left,
        plot_width,
        slot,
        count,
    };
    *geom_cell.borrow_mut() = Some(geometry);

    let hot = hover.filter(|&i| i < count).map(|i| Bounds {
        origin: bounds.origin + point(px(margin_left + slot * i as f32), px(MARGIN_TOP)),
        size: gpui::size(px(slot), px(plot_height)),
    });

    let mut bars = Vec::new();
    for (i, bucket) in data.buckets.iter().enumerate() {
        let x = margin_left + slot * (i as f32 + 0.5) - bar_width / 2.;
        let mut y0 = 0.;
        for (j, &part) in bucket.parts.iter().enumerate() {
            if part <= 0. {
                continue;
            }
            let (color, key) = match data.categories.get(j) {
                Some(category) => (category.color, category.key.clone().into()),
                None => (gpui::rgb(FAINT).into(), SharedString::default()),
            };
            let y_top = y_of(y0 + part);
            let seg_height = (y_of(y0) - y_top).max(0.5);
            bars.push(BarSegment {
                bounds: Bounds {
                    origin: bounds.origin + point(px(x), px(y_top)),
                    size: gpui::size(px(bar_width), px(seg_height)),
                },
                color,
                key,
            });
            y0 += part;
        }
    }

    Prepainted {
        geometry,
        grid,
        y_labels,
        x_ticks,
        bars,
        hot,
    }
}

/// Chart paint: dashed grid, hover band, stacked bars, axis labels.
fn paint_chart(
    bounds: Bounds<Pixels>,
    pre: Prepainted,
    grid_color: Hsla,
    hot_color: Hsla,
    emphasize: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    let geometry = pre.geometry;
    let plot_left = geometry.margin_left;
    let plot_right = geometry.margin_left + geometry.plot_width;

    for y in &pre.grid {
        // stroke-dasharray 2 3: 2px dash, 3px gap.
        let mut x = plot_left;
        while x < plot_right {
            window.paint_quad(gpui::quad(
                Bounds {
                    origin: bounds.origin + point(px(x), px(*y)),
                    size: gpui::size(px(2f32.min(plot_right - x)), px(1.)),
                },
                Corners::default(),
                grid_color,
                gpui::Edges::default(),
                Hsla::transparent_black(),
                gpui::BorderStyle::default(),
            ));
            x += 5.;
        }
    }

    if let Some(hot) = pre.hot {
        window.paint_quad(gpui::quad(
            hot,
            Corners::all(px(3.)),
            hot_color,
            gpui::Edges::default(),
            Hsla::transparent_black(),
            gpui::BorderStyle::default(),
        ));
    }

    for bar in &pre.bars {
        let dimmed = emphasize.is_some_and(|key| !bar.key.is_empty() && bar.key.as_ref() != key);
        let color = if dimmed {
            bar.color.opacity(0.22)
        } else {
            bar.color
        };
        window.paint_quad(gpui::quad(
            bar.bounds,
            Corners::all(px(1.)),
            color,
            gpui::Edges::default(),
            Hsla::transparent_black(),
            gpui::BorderStyle::default(),
        ));
    }

    for (line, origin) in pre.y_labels.iter().chain(&pre.x_ticks) {
        line.paint(
            *origin,
            px(AXIS_LINE_HEIGHT),
            TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();
    }
}

/// The hover tooltip: bucket title, one row per category
/// with a value, then the total with the request count.
fn tooltip(
    data: &TrendData,
    metric: TrendMetric,
    bucket: usize,
    geometry: TrendGeometry,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let b = &data.buckets[bucket];

    let mut rows: Vec<(Hsla, String, f64)> = data
        .categories
        .iter()
        .enumerate()
        .filter(|&(i, _)| b.parts[i] > 0.)
        .map(|(i, c)| (c.color, c.key.clone(), b.parts[i]))
        .collect();
    let stacked: f64 = rows.iter().map(|(_, _, v)| v).sum();
    if data.other.is_some() {
        let rest = b.total - stacked;
        if rest > 1e-9 {
            rows.push((gpui::rgb(FAINT).into(), "其他".to_string(), rest));
        }
    }
    rows.sort_by(|a, b| b.2.total_cmp(&a.2));

    let mut tip = div()
        .absolute()
        .top(px(2.))
        .min_w(px(172.))
        .max_w(px(260.))
        .px(px(10.))
        .py(px(7.))
        .rounded(px(9.))
        .bg(theme.popover)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_size(px(11.5))
        .child(
            div()
                .mb_1()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(b.label.clone()),
        );

    for (color, name, value) in rows {
        tip = tip.child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(div().size(px(7.)).rounded(px(2.)).flex_none().bg(color))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(div().ml_auto().pl_3().child(metric.format_long(value))),
        );
    }

    let mut sum = metric.format_long(b.total);
    if metric != TrendMetric::Requests && b.requests > 0 {
        sum.push_str(&format!(" · {} 请求", b.requests));
    }
    tip.child(
        div()
            .mt_1()
            .pt_1()
            .border_t_1()
            .border_color(theme.border)
            .text_color(theme.muted_foreground)
            .flex()
            .child(div().child("合计"))
            .child(div().ml_auto().child(sum)),
    )
    // Keep right edge inside the plot: flip left of the cursor when close.
    .left(tooltip_x(geometry, bucket))
}

/// Tooltip x: right of the slot center, flipped left when it would overflow
/// (the tip is assumed at its 260px max width).
fn tooltip_x(geometry: TrendGeometry, bucket: usize) -> Pixels {
    let center = geometry.margin_left + geometry.slot * (bucket as f32 + 0.5);
    const TIP_WIDTH: f32 = 260.;
    if center + 14. + TIP_WIDTH > geometry.margin_left + geometry.plot_width {
        px((center - TIP_WIDTH - 14.).max(0.))
    } else {
        px(center + 14.)
    }
}
