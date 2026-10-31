//! Quota view models and their rendering: the vendor-probed plan windows
//! shown on an account card (meter: window name +
//! remaining share above the bar, reset time + countdown below; one full-width
//! meter per row). The bar shows the REMAINING share (full = healthy),
//! band-colored by the used share. The prepaid balance view lives in the
//! `balance` module and is carried here as part of the key's reading.
//!
//! Windows spanning a week or more (weekly / monthly) also render a cumulative
//! trend chart under their meter (drawn by the `trend` module). The
//! projection needs only the current used share and the elapsed fraction;
//! history shapes the curve, never the forecast, so a freshly deployed
//! gateway shows the full diagonal alone while its series fills in.

use std::rc::Rc;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use gpui::{App, Div, ParentElement, Styled, px, relative};
use gpui_kit::component::ActiveTheme;

use gateway::quota::{KeyQuota, QuotaWindow};
use gateway::store::{BalanceHistoryPoint, QuotaHistoryPoint};

use super::balance::BalanceView;
use super::trend;
use super::trend::{Trend, TrendPoint, short_date};

/// Trend charts are drawn for windows at least this long: daily samples are
/// meaningless for the rolling 5-hour window (it resets several times a day).
const TREND_MIN_PERIOD_MINUTES: i64 = 7 * 1440;

/// Severity band of a quota meter, from the usage percentage: <70% green,
/// 70–<90% yellow, ≥90% red.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum QuotaBand {
    Ok,
    Warn,
    Full,
}

impl QuotaBand {
    pub(super) fn from_pct(used_pct: f64) -> Self {
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
    /// renders "remaining/total" instead of a bare percent.
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
    pub(super) balance: Option<BalanceView>,
    /// Display-ready info lines (plan, expiry, usage counters).
    pub(super) details: Vec<String>,
    /// When this reading was probed (snapshot time; a live refresh stamps now).
    pub(super) probed_at: chrono::DateTime<chrono::Utc>,
}

impl KeyQuotaView {
    /// Build the view from a reading plus the gateway's recorded samples for
    /// this key: quota samples (grouped by window name) and the current
    /// balance segment. Empty slices are fine — the charts then show the
    /// current point and the projection alone.
    pub(super) fn new(
        quota: KeyQuota,
        probed_at: chrono::DateTime<chrono::Utc>,
        history: &[QuotaHistoryPoint],
        balance_history: &[BalanceHistoryPoint],
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
            // A non-finite amount (vendor garbage) is no reading at all.
            balance: quota
                .balance
                .filter(|balance| balance.amount.is_finite())
                .map(|balance| BalanceView::new(balance, balance_history, probed_at)),
            details: quota.details,
            probed_at,
        }
    }

    /// Replace the readings with a live probe, keeping each window's recorded
    /// history and the balance segment (the live endpoint returns no series;
    /// a window or balance that was not present before starts its series now).
    pub(super) fn live(self, quota: KeyQuota, probed_at: chrono::DateTime<chrono::Utc>) -> Self {
        let mut histories: std::collections::HashMap<String, Vec<(DateTime<Utc>, f64)>> = self
            .windows
            .into_iter()
            .map(|window| (window.name, window.history))
            .collect();
        let reading = quota.balance.filter(|balance| balance.amount.is_finite());
        let balance = match (self.balance, reading) {
            (Some(previous), Some(balance)) => Some(previous.live(balance, probed_at)),
            (None, Some(balance)) => Some(BalanceView::new(balance, &[], probed_at)),
            (_, None) => None,
        };
        Self {
            windows: quota
                .windows
                .into_iter()
                .map(|window| {
                    let history = histories.remove(&window.name).unwrap_or_default();
                    QuotaWindowView::new(window, history)
                })
                .collect(),
            balance,
            details: quota.details,
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
                                format!("{remaining_amount}/{total}")
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

/// The window's chart input. `None` when the window is too short to chart,
/// has no reset instant (nothing to project onto), or carries no period.
fn window_trend(window: &QuotaWindowView, probed_at: DateTime<Utc>) -> Option<Trend> {
    let end = window.resets_at?;
    let period = window.period_minutes?;
    if period < TREND_MIN_PERIOD_MINUTES {
        return None;
    }
    // `period` is an approximation (a vendor month is 28–31 days), so use
    // it only to locate the current cycle. The plotted cycle begins at the
    // first observed use within that cycle, not at the calculated reset
    // boundary. A sample-to-sample drop marks a renewal; otherwise the
    // first sample at or after the calculated boundary is the best observed
    // start of the current cycle.
    let cycle_start = end - Duration::minutes(period);
    let mut last_drop: Option<DateTime<Utc>> = None;
    let mut previous_pct: Option<f64> = None;
    for (at, pct) in &window.history {
        if previous_pct.is_some_and(|previous| *pct < previous) {
            last_drop = Some(*at);
        }
        previous_pct = Some(*pct);
    }
    let cycle_start = last_drop.unwrap_or(cycle_start);
    let start = window
        .history
        .iter()
        .find_map(|(at, _)| (*at >= cycle_start).then_some(*at))
        .unwrap_or_else(|| probed_at.max(cycle_start).min(end));
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
            let share = ((100. - window.used_pct) / (projected - window.used_pct)).clamp(0., 1.);
            Some(now + Duration::seconds((remaining * share) as i64))
        }
        _ => None,
    };

    Some(Trend {
        start,
        end,
        now,
        used_pct: window.used_pct,
        points,
        forecast,
        crossing,
    })
}

/// The chart's two rows: a header (cumulative / projected) and the canvas.
fn trend_section(window: &QuotaWindowView, probed_at: DateTime<Utc>, cx: &App) -> Option<Div> {
    let trend = Rc::new(window_trend(window, probed_at)?);
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

    Some(
        gpui::div()
            .flex()
            .flex_col()
            .flex_none()
            .child(header)
            .child(trend::chart(trend)),
    )
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
/// share, rendered as "remaining/total" ("$69.99/$70.00"). Falls back to the
/// total itself when the total string is not a plain number (keeps the unit
/// prefix/suffix as-is).
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
