//! The prepaid balance on an account card: a meter (remaining / segment base)
//! and a depletion trend chart drawn by the `trend` module.
//!
//! The chart covers the current balance segment only. A segment opens at the
//! first recorded reading or at a top-up; vendors expose no top-up events, so
//! a top-up is simply an increase between readings (and a lower bound when
//! spend happened in the same probe interval). The segment's opening amount is
//! the 100% base: the y-axis is the spent share of it.
//!
//! The forecast is the straight-line spend rate since the segment opened,
//! extended to the instant the balance reaches zero — the chart's end-point
//! sits exactly at 100% spent. A rate that would take longer than
//! [`FORECAST_CAP_DAYS`] to empty the balance is not a forecast worth dating:
//! the chart then stops at the cap and the header says so.

use std::rc::Rc;

use chrono::{DateTime, Duration, Utc};
use gpui::{App, Div, ParentElement, Styled, px, relative};
use gpui_kit::component::ActiveTheme;

use gateway::quota::{Balance, BalanceStep};
use gateway::store::BalanceHistoryPoint;

use super::quota::QuotaBand;
use super::trend;
use super::trend::{Trend, TrendPoint, short_date};

/// Depletion further out than this is charted to the cap, not dated.
const FORECAST_CAP_DAYS: i64 = 180;
/// A depletion date closer than this renders in the danger color.
const URGENT_DAYS: i64 = 3;

/// A key's balance reading plus its current segment of recorded amounts.
#[derive(Clone, Debug)]
pub(super) struct BalanceView {
    balance: Balance,
    /// The current segment's amounts, oldest first: the opener (the base)
    /// first, the current reading folded in last. Never empty once built;
    /// the opener's instant is when the segment (or the top-up) was seen.
    segment: Vec<(DateTime<Utc>, f64)>,
    /// The increase that opened the segment; `None` when it is the key's
    /// first record.
    topup: Option<f64>,
}

/// When the balance runs out, as far as the straight-line rate says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Depletion {
    /// Too little elapsed time or no spend yet: no forecast.
    Unknown,
    /// Further out than [`FORECAST_CAP_DAYS`].
    Capped,
    /// The projected instant the balance reaches zero.
    At(DateTime<Utc>),
    /// Already at (or below) zero.
    Empty,
}

impl BalanceView {
    /// Build from a reading and this key's recorded segment (oldest first,
    /// opener first; an empty slice starts a segment at this reading).
    pub(super) fn new(
        balance: Balance,
        history: &[BalanceHistoryPoint],
        probed_at: DateTime<Utc>,
    ) -> Self {
        let topup = history
            .first()
            .filter(|point| point.opens_segment)
            .and_then(|point| point.topup);
        let mut view = Self {
            segment: history
                .iter()
                .map(|point| (point.probed_at, point.amount))
                .collect(),
            topup,
            balance,
        };
        view.fold(probed_at);
        view
    }

    /// Replace the reading with a live probe, folding it into the segment.
    /// The live stamp is the GUI clock while the segment carries gateway
    /// times; it never goes before the segment's last point, so a GUI clock
    /// behind the gateway's cannot drop the reading from the chart.
    pub(super) fn live(mut self, balance: Balance, probed_at: DateTime<Utc>) -> Self {
        let probed_at = self.segment.last().map_or(probed_at, |(at, _)| {
            probed_at.max(*at + Duration::seconds(1))
        });
        self.balance = balance;
        self.fold(probed_at);
        self
    }

    /// Fold the current reading into the segment with the gateway's rule: an
    /// increase opens a new segment, a decrease extends it.
    fn fold(&mut self, probed_at: DateTime<Utc>) {
        let amount = self.balance.amount;
        let last = self.segment.last().copied();
        match gateway::quota::classify(last.map(|(_, previous)| previous), amount) {
            BalanceStep::Opened { topup } => {
                self.segment.clear();
                self.segment.push((probed_at, amount));
                self.topup = topup;
            }
            BalanceStep::Spent => {
                if last.is_none_or(|(at, _)| probed_at > at) {
                    self.segment.push((probed_at, amount));
                }
            }
            BalanceStep::Unchanged => {}
        }
    }

    /// The segment opener: when it opened and its amount (the 100% base).
    fn opener(&self) -> Option<(DateTime<Utc>, f64)> {
        self.segment.first().copied()
    }

    /// Spent share of a positive base, 0–100.
    fn spent_pct(base: f64, amount: f64) -> f64 {
        ((base - amount) / base * 100.).clamp(0., 100.)
    }
}

/// The chart input and depletion estimate. `None` without a positive base
/// (nothing to take shares of): the meter renders alone.
fn balance_trend(view: &BalanceView, probed_at: DateTime<Utc>) -> Option<(Trend, Depletion)> {
    let (start, base) = view.opener()?;
    if base <= 0. {
        return None;
    }
    // Never before the segment's last point (gateway clock may lead ours).
    let last_at = view.segment.last().map_or(start, |(at, _)| *at);
    let now = probed_at.max(last_at);
    let used_pct = BalanceView::spent_pct(base, view.balance.amount);

    let mut points: Vec<TrendPoint> = view
        .segment
        .iter()
        .filter(|(at, _)| *at <= now)
        .map(|(at, amount)| TrendPoint {
            at: *at,
            pct: BalanceView::spent_pct(base, *amount),
        })
        .collect();
    if points.last().is_none_or(|point| point.at < now) {
        points.push(TrendPoint {
            at: now,
            pct: used_pct,
        });
    }

    let elapsed = now - start;
    let (end, forecast, crossing, depletion) = if used_pct >= 100. {
        let end = now.max(start + Duration::hours(1));
        (end, Some(100.), None, Depletion::Empty)
    } else if elapsed < Duration::hours(1) || used_pct <= 0. {
        // At least a day of span so the axis is never degenerate.
        let end = now.max(start + Duration::days(1));
        (end, None, None, Depletion::Unknown)
    } else {
        let rate = used_pct / elapsed.num_seconds() as f64;
        let to_empty = (100. - used_pct) / rate;
        let cap = Duration::days(FORECAST_CAP_DAYS);
        if to_empty > cap.num_seconds() as f64 {
            let projected = used_pct + rate * cap.num_seconds() as f64;
            (now + cap, Some(projected), None, Depletion::Capped)
        } else {
            let end = now + Duration::seconds(to_empty.ceil() as i64);
            (end, Some(100.), None, Depletion::At(end))
        }
    };

    Some((
        Trend {
            start,
            end,
            now,
            used_pct,
            points,
            forecast,
            crossing,
        },
        depletion,
    ))
}

/// The balance meter: label + "remaining/base" above the bar, the depletion
/// chart (when the segment has a positive base), and the segment's origin
/// (top-up or first record) below.
pub(super) fn balance_row(view: &BalanceView, probed_at: DateTime<Utc>, cx: &App) -> Div {
    let theme = cx.theme();
    let balance = &view.balance;
    let base = view.opener().filter(|(_, base)| *base > 0.);
    let used_pct = match base {
        Some((_, base)) => BalanceView::spent_pct(base, balance.amount),
        None if balance.amount > 0. => 0.,
        None => 100.,
    };
    let remaining = 100. - used_pct;
    let fill_color = match QuotaBand::from_pct(used_pct) {
        QuotaBand::Ok => theme.success,
        QuotaBand::Warn => theme.warning,
        QuotaBand::Full => theme.danger,
    };
    let amount_label = match base {
        Some((_, base)) => format!("{balance}/{}{base:.2}", balance.symbol),
        None => balance.to_string(),
    };
    let display_tz = crate::ui_tz::display_tz();
    let origin_line = view.opener().map(|(opened_at, _)| {
        let date = short_date(opened_at.with_timezone(&display_tz));
        match view.topup {
            Some(topup) => format!("{date} 充值 +{}{topup:.2}", balance.symbol),
            None => format!("{date} 起记录"),
        }
    });

    gpui::div()
        .w_full()
        .min_w_0()
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
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("余额"),
                )
                .child(gpui::div().flex_1())
                .child(
                    gpui::div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(amount_label),
                ),
        )
        // Fill = remaining share, band-colored by the spent share (same gauge
        // as the quota meters).
        .child(
            gpui::div().h(px(4.)).rounded_full().bg(theme.muted).child(
                gpui::div()
                    .h_full()
                    .rounded_full()
                    .bg(fill_color)
                    .w(relative(remaining as f32 / 100.)),
            ),
        )
        .children(trend_section(view, probed_at, cx))
        .child(
            gpui::div()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .children(origin_line),
        )
}

/// Header (spent amount / depletion estimate) plus the chart canvas.
fn trend_section(view: &BalanceView, probed_at: DateTime<Utc>, cx: &App) -> Option<Div> {
    let (trend, depletion) = balance_trend(view, probed_at)?;
    let theme = cx.theme();
    let (_, base) = view.opener()?;
    let spent = (base - view.balance.amount).max(0.);
    let (forecast_text, forecast_color) = match depletion {
        Depletion::Unknown => ("预计 —".to_string(), theme.muted_foreground),
        Depletion::Capped => (
            format!("预计 >{FORECAST_CAP_DAYS} 天"),
            theme.muted_foreground,
        ),
        Depletion::Empty => ("已耗尽".to_string(), theme.danger),
        Depletion::At(end) => {
            let color = if end - trend.now < Duration::days(URGENT_DAYS) {
                theme.danger
            } else {
                theme.muted_foreground
            };
            let date = short_date(end.with_timezone(&crate::ui_tz::display_tz()));
            (format!("预计 {date} 耗尽"), color)
        }
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
                .child(format!("已用 {}{spent:.2}", view.balance.symbol)),
        )
        .child(gpui::div().pt_0p5().flex_1())
        .child(gpui::div().text_color(forecast_color).child(forecast_text));

    Some(
        gpui::div()
            .flex()
            .flex_col()
            .flex_none()
            .child(header)
            .child(trend::chart(Rc::new(trend))),
    )
}
