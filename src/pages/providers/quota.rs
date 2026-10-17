//! Quota view models and their rendering: the vendor-probed plan windows and
//! prepaid balance shown on an account card (meter: window name +
//! remaining share above the bar, reset time + countdown below; one full-width
//! meter per row). The bar shows the REMAINING share (full = healthy),
//! band-colored by the used share. No burn-rate projection — extrapolation
//! needs historical samples we don't store, so we don't fake one.

use chrono::{Datelike, Timelike};
use gpui::{App, Div, ParentElement, Styled, px, relative};
use gpui_kit::component::ActiveTheme;

use gateway::quota::{KeyQuota, QuotaWindow};

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
    /// The window's full allowance ("$70.00") for a money-window, so the meter
    /// renders "used / total" instead of a bare percent.
    total: Option<String>,
}

impl QuotaWindowView {
    fn band(&self) -> QuotaBand {
        QuotaBand::from_pct(self.used_pct)
    }
}

impl From<QuotaWindow> for QuotaWindowView {
    fn from(window: QuotaWindow) -> Self {
        Self {
            name: window.name,
            used_pct: window.used_pct,
            resets_at: window.resets_at,
            total: window.total,
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
    pub(super) fn new(quota: KeyQuota, probed_at: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            windows: quota
                .windows
                .into_iter()
                .map(QuotaWindowView::from)
                .collect(),
            balance: quota.balance,
            probed_at,
        }
    }
}

/// Quota windows, one full-width meter per row: name + remaining share above
/// the bar, reset time + countdown below. The bar fill is the REMAINING share.
pub(super) fn quota_row(windows: &[QuotaWindowView], cx: &App) -> Div {
    gpui::div()
        .flex()
        .flex_col()
        .gap_3()
        .children(windows.iter().map(|window| quota_meter(window, cx)))
}

/// One window column: labels, bar, reset line.
fn quota_meter(window: &QuotaWindowView, cx: &App) -> Div {
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
