//! Quota view models and their rendering: the vendor-probed plan windows and
//! prepaid balance shown on an account card (meter: window name +
//! remaining share above the bar, reset time + countdown below; one full-width
//! meter per row). The bar shows the REMAINING share (full = healthy),
//! band-colored by the used share.
//!
//! Windows spanning a week or more (weekly / monthly) also render a cumulative
//! trend chart under their meter: the recorded samples solid over the used
//! time span, a dashed join from 0% at the window start to where the series
//! begins, and a dashed projection from the last sample to the reset — the
//! straight-line estimate of the current rate. The y-axis always tops at the
//! 100% allowance: a projection past it pins there and runs flat to the reset.
//! The projection needs only the current used share and the elapsed fraction;
//! history shapes the curve, never the forecast, so a freshly deployed
//! gateway shows the full diagonal alone while its series fills in.

use std::rc::Rc;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use gpui::{
    App, BorderStyle, Bounds, Corners, Div, Edges, Hsla, ParentElement, Path, PathBuilder, Pixels,
    Point, ShapedLine, SharedString, Styled, TextAlign, TextRun, Window, canvas, point, px, quad,
    relative, size,
};
use gpui_kit::component::ActiveTheme;

use gateway::quota::{KeyQuota, QuotaWindow};
use gateway::store::QuotaHistoryPoint;

/// Trend charts are drawn for windows at least this long: daily samples are
/// meaningless for the rolling 5-hour window (it resets several times a day).
const TREND_MIN_PERIOD_MINUTES: i64 = 7 * 1440;

/// Chart element height: plot + one label row.
const TREND_HEIGHT: f32 = 88.;
/// The chart's y-axis top: the window allowance is the story, so the scale
/// never stretches past it — a projection past 100% pins there and runs flat.
const TREND_TOP_PCT: f64 = 100.;
const TREND_TOP: f32 = 4.;
const TREND_LABEL_ROW: f32 = 13.;
const TREND_RIGHT: f32 = 4.;
const AXIS_FONT: f32 = 9.5;
const AXIS_LINE_HEIGHT: f32 = 12.;

/// Severity band of a quota meter, from the usage percentage: <70% green,
/// 70–<90% yellow, ≥90% red.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum QuotaBand {
    Ok,
    Warn,
    Full,
}

impl QuotaBand {
    fn from_pct(used_pct: f64) -> Self {
        if used_pct >= 90. {
            QuotaBand::Full
        } else if used_pct >= 70. {
            QuotaBand::Warn
        } else {
            QuotaBand::Ok
        }
    }
}

/// One quota window of a vendor plan as rendered on a card (converted from
/// the gateway's probed reading).
#[derive(Clone, Debug)]
pub(super) struct QuotaWindowView {
    /// Display name from the probe ("5 小时", "本周").
    name: String,
    /// Share of the window already used, 0–100.
    used_pct: f64,
    /// When the window resets, when the vendor says so.
    resets_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Approximate window length in minutes (sort key, and the trend chart's
    /// x-axis span). `None` in snapshots written before the field existed.
    period_minutes: Option<i64>,
    /// The window's full allowance ("$70.00") for a money-window, so the meter
    /// renders "used / total" instead of a bare percent.
    total: Option<String>,
    /// Probe samples recorded for this window name, oldest first — all of the
    /// gateway's 35-day series, including a previous window's points; the
    /// chart trims them to the current window.
    history: Vec<(chrono::DateTime<chrono::Utc>, f64)>,
}

impl QuotaWindowView {
    fn band(&self) -> QuotaBand {
        QuotaBand::from_pct(self.used_pct)
    }

    fn new(window: QuotaWindow, history: Vec<(chrono::DateTime<chrono::Utc>, f64)>) -> Self {
        Self {
            name: window.name,
            used_pct: window.used_pct,
            resets_at: window.resets_at,
            period_minutes: window.period_minutes,
            total: window.total,
            history,
        }
    }
}

/// A key's quota section: plan windows and/or a prepaid balance, plus when
/// the reading was probed. The reading comes from the stored snapshot on
/// page load and from a live vendor probe on refresh; a key with neither
/// renders no quota section at all.
#[derive(Clone, Debug)]
pub(super) struct KeyQuotaView {
    pub(super) windows: Vec<QuotaWindowView>,
    pub(super) balance: Option<String>,
    /// When this reading was probed (snapshot time; a live refresh stamps now).
    pub(super) probed_at: chrono::DateTime<chrono::Utc>,
}

impl KeyQuotaView {
    /// Build the view from a reading plus the gateway's recorded samples for
    /// this key (grouped by window name; an empty slice is fine — the chart
    /// then shows the current point and the projection alone).
    pub(super) fn new(
        quota: KeyQuota,
        probed_at: chrono::DateTime<chrono::Utc>,
        history: &[QuotaHistoryPoint],
    ) -> Self {
        let mut by_window: std::collections::HashMap<&str, Vec<(chrono::DateTime<Utc>, f64)>> =
            std::collections::HashMap::new();
        for point in history {
            by_window
                .entry(point.window_name.as_str())
                .or_default()
                .push((point.probed_at, point.used_pct));
        }
        Self {
            windows: quota
                .windows
                .into_iter()
                .map(|window| {
                    let history = by_window
                        .get(window.name.as_str())
                        .cloned()
                        .unwrap_or_default();
                    QuotaWindowView::new(window, history)
                })
                .collect(),
            balance: quota.balance,
            probed_at,
        }
    }

    /// Replace the readings with a live probe, keeping each window's recorded
    /// history (the live endpoint returns no series; a window that was not
    /// present before simply starts its series now).
    pub(super) fn live(self, quota: KeyQuota, probed_at: chrono::DateTime<chrono::Utc>) -> Self {
        let mut histories: std::collections::HashMap<String, Vec<(DateTime<Utc>, f64)>> = self
            .windows
            .into_iter()
            .map(|window| (window.name, window.history))
            .collect();
        Self {
            windows: quota
                .windows
                .into_iter()
                .map(|window| {
                    let history = histories.remove(&window.name).unwrap_or_default();
                    QuotaWindowView::new(window, history)
                })
                .collect(),
            balance: quota.balance,
            probed_at,
        }
    }
}

/// Quota windows, one full-width meter per row: name + remaining share above
/// the bar, reset time + countdown below. The bar fill is the REMAINING share;
/// long windows also carry a cumulative trend chart under the bar.
pub(super) fn quota_row(
    windows: &[QuotaWindowView],
    probed_at: chrono::DateTime<chrono::Utc>,
    cx: &App,
) -> Div {
    gpui::div().flex().flex_col().gap_3().children(
        windows
            .iter()
            .map(|window| quota_meter(window, probed_at, cx)),
    )
}

/// One window column: labels, bar, trend (long windows), reset line.
fn quota_meter(
    window: &QuotaWindowView,
    probed_at: chrono::DateTime<chrono::Utc>,
    cx: &App,
) -> Div {
    let remaining = (100. - window.used_pct).clamp(0., 100.);
    let fill_color = match window.band() {
        QuotaBand::Ok => cx.theme().success,
        QuotaBand::Warn => cx.theme().warning,
        QuotaBand::Full => cx.theme().danger,
    };
    gpui::div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        // Above the bar: window name left, remaining share (or "remaining / total"
        // for a money-window) right.
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(window.name.clone()),
                )
                .child(gpui::div().flex_1())
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(match &window.total {
                            Some(total) => {
                                let remaining_amount = remaining_amount(total, remaining);
                                format!("剩余 {remaining_amount} / {total}")
                            }
                            None => format!("剩余 {}", pct_label(remaining)),
                        }),
                ),
        )
        // The bar: fill width = remaining share, band-colored by the used share.
        // 4px, same gauge as the overview card bars (usage.rs).
        .child(
            gpui::div()
                .h(px(4.))
                .rounded_full()
                .bg(cx.theme().muted)
                .child(
                    gpui::div()
                        .h_full()
                        .rounded_full()
                        .bg(fill_color)
                        .w(relative(remaining as f32 / 100.)),
                ),
        )
        // The cumulative trend + forecast (weekly / monthly only): the chart
        // sits between the bar and the reset line so the reading hierarchy
        // stays name → share → shape → reset.
        .children(trend_section(window, probed_at, cx))
        // Below the bar: when the window resets, when the vendor says so — a
        // money-window has no reset instant, so the line simply stays empty.
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .children(
                    window
                        .resets_at
                        .map(|resets_at| reset_line(resets_at, chrono::Utc::now())),
                ),
        )
}

// ---------- trend chart (cumulative series + straight-line forecast) ----------

/// One plotted sample: the window's cumulative used share at an instant.
#[derive(Clone, Debug)]
struct TrendPoint {
    at: DateTime<Utc>,
    pct: f64,
}

/// Everything the chart paints, derived once per render from the reading and
/// its recorded samples. The projection is pure arithmetic:
/// `used_pct / elapsed_fraction`, extended to the window's end.
struct Trend {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    /// The reading instant, clamped into the window.
    now: DateTime<Utc>,
    /// Current used share (the projection's anchor).
    used_pct: f64,
    /// Samples within the current window, oldest first; the current reading is
    /// always the last point.
    points: Vec<TrendPoint>,
    /// Projected share at the window's end; `None` before an hour of elapsed
    /// window (extrapolating from minutes produces noise, not a forecast).
    forecast: Option<f64>,
    /// When the projection pins at the 100% cap (already over the limit counts
    /// as now).
    crossing: Option<DateTime<Utc>>,
}

impl Trend {
    /// `None` when the window is too short to chart, has no reset instant
    /// (nothing to project onto), or carries no period.
    fn prepare(window: &QuotaWindowView, probed_at: DateTime<Utc>) -> Option<Self> {
        let end = window.resets_at?;
        let period = window.period_minutes?;
        if period < TREND_MIN_PERIOD_MINUTES {
            return None;
        }
        // `period` is an approximation (a vendor month is 28–31 days), so the
        // arithmetic start can miss the window's birth by a day in either
        // direction — fatal for a fresh account, whose derived start can land
        // in the future, past every recorded sample. Observed evidence
        // supersedes the arithmetic: a sample-to-sample drop is a renewal
        // (usage only climbs within a window) and pins the birth; a series
        // with no drop that begins before the derived start belongs to this
        // window's birth.
        let mut start = end - Duration::minutes(period);
        let mut last_drop: Option<DateTime<Utc>> = None;
        let mut previous_pct: Option<f64> = None;
        for (at, pct) in &window.history {
            if previous_pct.is_some_and(|previous| *pct < previous) {
                last_drop = Some(*at);
            }
            previous_pct = Some(*pct);
        }
        start = match last_drop {
            Some(renewed_at) => renewed_at,
            None => match window.history.first() {
                Some((first_at, _)) if *first_at < start => *first_at,
                _ => start,
            },
        };
        if end <= start {
            return None;
        }
        let now = probed_at.max(start).min(end);
        let span_seconds = (end - start).num_seconds().max(1) as f64;
        let elapsed = (now - start).num_seconds() as f64 / span_seconds;
        let forecast = ((now - start) >= Duration::hours(1) && elapsed > 0.)
            .then(|| (window.used_pct / elapsed).min(999.));

        let mut points: Vec<TrendPoint> = window
            .history
            .iter()
            .filter(|(at, _)| *at >= start && *at <= now)
            .map(|(at, pct)| TrendPoint { at: *at, pct: *pct })
            .collect();
        if points.last().is_none_or(|point| point.at < now) {
            points.push(TrendPoint {
                at: now,
                pct: window.used_pct,
            });
        }

        let crossing = match forecast {
            Some(_) if window.used_pct >= 100. => Some(now),
            Some(projected) if projected > 100. => {
                let remaining = (end - now).num_seconds() as f64;
                let share =
                    ((100. - window.used_pct) / (projected - window.used_pct)).clamp(0., 1.);
                Some(now + Duration::seconds((remaining * share) as i64))
            }
            _ => None,
        };

        Some(Self {
            start,
            end,
            now,
            used_pct: window.used_pct,
            points,
            forecast,
            crossing,
        })
    }
}

/// The chart's two rows: a header (cumulative / projected) and the canvas.
fn trend_section(window: &QuotaWindowView, probed_at: DateTime<Utc>, cx: &App) -> Option<Div> {
    let trend = Rc::new(Trend::prepare(window, probed_at)?);
    let theme = cx.theme();
    let (forecast_text, forecast_color) = match trend.forecast {
        Some(projected) if projected > 100. => {
            let text = match trend.crossing {
                Some(at) => format!(
                    "预计 {projected:.0}% · {} 超额",
                    short_date(at.with_timezone(&crate::ui_tz::display_tz()))
                ),
                None => format!("预计 {projected:.0}%"),
            };
            (text, theme.danger)
        }
        Some(projected) => (format!("预计 {projected:.0}%"), theme.muted_foreground),
        None => ("预计 —".to_string(), theme.muted_foreground),
    };

    let header = gpui::div()
        .flex()
        .items_center()
        .gap_1()
        .text_size(px(10.5))
        .child(
            gpui::div()
                .py_1()
                .text_color(theme.muted_foreground)
                .child(format!("累计 {:.0}%", trend.used_pct)),
        )
        .child(gpui::div().pt_0p5().flex_1())
        .child(gpui::div().text_color(forecast_color).child(forecast_text));

    let input = Rc::clone(&trend);
    let chart = canvas(
        move |bounds, window, cx| prepaint_trend(bounds, &input, window, cx),
        |_bounds, pre, window, cx| paint_trend(pre, window, cx),
    );

    Some(
        gpui::div()
            .flex()
            .flex_col()
            .flex_none()
            .child(header)
            .child(
                gpui::div()
                    .h(px(TREND_HEIGHT))
                    .w_full()
                    .child(chart.size_full()),
            ),
    )
}

/// Chart geometry and paths, absolute in window space (prepaint folds the
/// bounds origin in), so painting is a straight walk.
struct TrendPaint {
    plot_left: f32,
    plot_right: f32,
    baseline: f32,
    grid: Vec<f32>,
    y_labels: Vec<(ShapedLine, Point<Pixels>)>,
    x_labels: Vec<(ShapedLine, Point<Pixels>)>,
    area: Option<Path<Pixels>>,
    usage: Option<Path<Pixels>>,
    /// The dashed uniform-rate reference across the whole window.
    plan: Option<Path<Pixels>>,
}

fn prepaint_trend(
    bounds: Bounds<Pixels>,
    trend: &Trend,
    window: &mut Window,
    cx: &mut App,
) -> TrendPaint {
    let theme = cx.theme();
    let axis_color = theme.muted_foreground;
    let width: f32 = bounds.size.width.into();
    let height: f32 = bounds.size.height.into();
    let origin_x: f32 = bounds.origin.x.into();
    let origin_y: f32 = bounds.origin.y.into();
    let plot_top = origin_y + TREND_TOP;
    let baseline = origin_y + height - TREND_LABEL_ROW;
    let plot_h = (baseline - plot_top).max(1.);

    let font = window.text_style().font();
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

    let ticks = [0., 50., TREND_TOP_PCT];
    let y_lines: Vec<(ShapedLine, f64)> = ticks
        .iter()
        .map(|pct| (shape(window, &format!("{pct:.0}%")), *pct))
        .collect();
    let widest = y_lines
        .iter()
        .map(|(line, _)| f32::from(line.width()))
        .fold(0., f32::max);
    let plot_left = origin_x + widest + 4.;
    let plot_right = (origin_x + width - TREND_RIGHT).max(plot_left + 1.);

    let span = (trend.end - trend.start).num_seconds().max(1) as f32;
    let x_of = |at: DateTime<Utc>| {
        let rel = ((at - trend.start).num_seconds() as f32 / span).clamp(0., 1.);
        plot_left + (plot_right - plot_left) * rel
    };
    let y_of = |pct: f64| {
        let rel = (pct / TREND_TOP_PCT).clamp(0., 1.) as f32;
        plot_top + plot_h * (1. - rel)
    };

    let y_labels: Vec<(ShapedLine, Point<Pixels>)> = y_lines
        .into_iter()
        .map(|(line, pct)| {
            let w: f32 = line.width().into();
            (
                line,
                point(
                    px(plot_left - 4. - w),
                    px(y_of(pct) - AXIS_LINE_HEIGHT / 2.),
                ),
            )
        })
        .collect();

    let points: Vec<Point<Pixels>> = trend
        .points
        .iter()
        .map(|sample| point(px(x_of(sample.at)), px(y_of(sample.pct))))
        .collect();

    let (area, usage) = match (points.first(), points.last()) {
        (Some(first), Some(last)) => {
            let area = build_path(PathBuilder::fill(), |builder| {
                builder.move_to(point(first.x, px(y_of(0.))));
                for at in &points {
                    builder.line_to(*at);
                }
                builder.line_to(point(last.x, px(y_of(0.))));
                builder.close();
            });
            let usage = build_path(PathBuilder::stroke(px(1.5)), |builder| {
                builder.move_to(*first);
                for at in points.iter().skip(1) {
                    builder.line_to(*at);
                }
            });
            (area, usage)
        }
        _ => (None, None),
    };

    // Dashed connectors around the observed span: from 0% at the window start
    // to the first sample (no readings exist before the series began), and
    // from the last sample to the reset. A projection past the limit pins at
    // 100% the moment it gets there and runs flat to the reset; under the
    // limit it lands wherever it lands. With no samples at all the reference
    // is the full uniform-rate diagonal.
    let plan = trend.forecast.and_then(|projected| {
        if projected <= 0. {
            return None;
        }
        build_path(
            PathBuilder::stroke(px(1.2)).dash_array(&[px(4.), px(3.)]),
            |builder| {
                // The run from `from` to the reset: to the projected share
                // under the limit, or up to the pin and flat at 100%.
                let emit = |builder: &mut PathBuilder, from_at: DateTime<Utc>, from_pct: f64| {
                    builder.move_to(point(px(x_of(from_at)), px(y_of(from_pct))));
                    if projected > TREND_TOP_PCT {
                        let crossing = trend.crossing.unwrap_or(from_at);
                        builder.line_to(point(px(x_of(crossing)), px(y_of(TREND_TOP_PCT))));
                        builder.line_to(point(px(x_of(trend.end)), px(y_of(TREND_TOP_PCT))));
                    } else {
                        builder.line_to(point(px(x_of(trend.end)), px(y_of(projected))));
                    }
                };
                match trend.points.first() {
                    None => emit(builder, trend.start, 0.),
                    Some(first) => {
                        // 0% at the window start, joined to where the series begins.
                        builder.move_to(point(px(x_of(trend.start)), px(y_of(0.))));
                        builder.line_to(point(px(x_of(first.at)), px(y_of(first.pct))));
                        // From the end of the observed usage, straight to the
                        // projection at the reset.
                        if projected > trend.used_pct && trend.used_pct < TREND_TOP_PCT {
                            emit(builder, trend.now, trend.used_pct);
                        }
                    }
                }
            },
        )
    });

    let now_x = x_of(trend.now);
    let mut x_labels = vec![(
        shape(
            window,
            &short_date(trend.start.with_timezone(&crate::ui_tz::display_tz())),
        ),
        point(px(plot_left), px(baseline + 2.)),
    )];
    // "今天" only when it clears both edge labels.
    if now_x - plot_left > 34. && plot_right - now_x > 34. {
        let line = shape(window, "今天");
        let w: f32 = line.width().into();
        x_labels.push((line, point(px(now_x - w / 2.), px(baseline + 2.))));
    }
    {
        let line = shape(
            window,
            &short_date(trend.end.with_timezone(&crate::ui_tz::display_tz())),
        );
        let w: f32 = line.width().into();
        x_labels.push((line, point(px(plot_right - w), px(baseline + 2.))));
    }

    TrendPaint {
        plot_left,
        plot_right,
        baseline,
        grid: ticks.iter().map(|pct| y_of(*pct)).collect(),
        y_labels,
        x_labels,
        area,
        usage,
        plan,
    }
}

/// Build a path from a builder callback; a build error (degenerate geometry)
/// paints nothing instead of failing the frame.
fn build_path(
    mut builder: PathBuilder,
    draw: impl FnOnce(&mut PathBuilder),
) -> Option<Path<Pixels>> {
    draw(&mut builder);
    builder.build().ok()
}

fn paint_trend(pre: TrendPaint, window: &mut Window, cx: &mut App) {
    let theme = cx.theme();

    // Dashed grid at the 50% steps; the 100% limit line is dashed but stronger.
    for (index, y) in pre.grid.iter().enumerate() {
        let color = if index == 2 {
            theme.border
        } else {
            theme.muted
        };
        let mut x = pre.plot_left;
        while x < pre.plot_right {
            window.paint_quad(quad(
                Bounds {
                    origin: point(px(x), px(*y)),
                    size: size(px(2f32.min(pre.plot_right - x)), px(1.)),
                },
                Corners::default(),
                color,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
            x += 5.;
        }
    }

    // Baseline.
    window.paint_quad(quad(
        Bounds {
            origin: point(px(pre.plot_left), px(pre.baseline)),
            size: size(px(pre.plot_right - pre.plot_left), px(1.)),
        },
        Corners::default(),
        theme.border,
        Edges::default(),
        Hsla::transparent_black(),
        BorderStyle::default(),
    ));

    // The dashed reference goes under everything data-shaped, so the solid
    // usage line reads on top of it.
    if let Some(plan) = pre.plan {
        window.paint_path(plan, theme.danger.opacity(0.75));
    }
    if let Some(area) = pre.area {
        window.paint_path(area, theme.success.opacity(0.10));
    }
    if let Some(usage) = pre.usage {
        window.paint_path(usage, theme.success);
    }

    for (line, origin) in pre.y_labels.iter().chain(&pre.x_labels) {
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

/// `MM-DD` in the display timezone (the chart's axis labels).
fn short_date(at: DateTime<chrono::FixedOffset>) -> String {
    format!("{:02}-{:02}", at.month(), at.day())
}

/// Remaining-share label: whole percentages render as "100%", fractional as
/// "45.0%".
fn pct_label(pct: f64) -> String {
    let pct = pct.clamp(0., 100.);
    if (pct * 10.).round() % 10. == 0. {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

/// The remaining amount of a money-window, from its total and the remaining
/// share: "剩余 $69.99 / $70.00". Falls back to the total itself when the
/// total string is not a plain number (keeps the unit prefix/suffix as-is).
fn remaining_amount(total: &str, remaining_pct: f64) -> String {
    let (prefix, digits) = total.split_at(total.find(|c: char| c.is_ascii_digit()).unwrap_or(0));
    let (num, suffix) = digits.split_at(
        digits
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(digits.len()),
    );
    let Some(amount) = num.parse::<f64>().ok() else {
        return total.to_string();
    };
    format!("{prefix}{:.2}{suffix}", amount * remaining_pct / 100.)
}

/// Reset line under a bar: "重置 21:40 · 4 小时后" when the reset is within
/// 24 hours, "重置 10月12日 17:40 · 2 天后" beyond that.
fn reset_line(
    resets_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let until = resets_at.signed_duration_since(now);
    let local = resets_at.with_timezone(&crate::ui_tz::display_tz());
    let time = if until <= chrono::Duration::hours(24) {
        format!("{:02}:{:02}", local.hour(), local.minute())
    } else {
        format!(
            "{}月{}日 {:02}:{:02}",
            local.month(),
            local.day(),
            local.hour(),
            local.minute()
        )
    };
    format!("重置 {time} · {}", until_label(until))
}

/// Countdown to the reset: "已到期" when past, then minutes / hours / days.
fn until_label(until: chrono::Duration) -> String {
    if until <= chrono::Duration::zero() {
        "已到期".to_string()
    } else if until < chrono::Duration::minutes(90) {
        format!("{} 分钟后", until.num_minutes().max(1))
    } else if until < chrono::Duration::hours(48) {
        format!("{} 小时后", (until.num_minutes() + 59) / 60)
    } else {
        format!("{} 天后", (until.num_hours() + 23) / 24)
    }
}
