//! The available-quota summary in the 账号 tab row: per provider, the summed
//! remaining allowance of every quota window that reports an absolute total,
//! plus the summed prepaid balances. Percent-only windows carry no amount and
//! are skipped.
//!
//! Derived on every render from the visible cards (the caller decides
//! visibility); nothing is cached. [`summarize`] is pure so the arithmetic is
//! testable without gpui.
//!
//! Rules:
//! - Windows aggregate by `(provider name, window name)`: remaining =
//!   total × (100 − used_pct)/100, totals sum too. A window whose reset
//!   instant has passed has renewed while its snapshot is stale, so it
//!   counts as full.
//! - A total that does not parse as a number is skipped. The first window
//!   seen fixes the unit prefix/suffix of its aggregate; a later window with
//!   a different unit cannot be added to it and is skipped.
//! - Balances aggregate by `(provider name, symbol)`.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use gpui::{App, Div, ParentElement, Styled, px};
use gpui_kit::component::ActiveTheme;

use super::quota::KeyQuotaView;
use super::quota::parse_amount;

/// One provider's summed quota: its windows in first-seen order, then its
/// balances in first-seen order. Never empty.
#[derive(Debug, PartialEq)]
pub(super) struct ProviderTotal<'a> {
    provider: &'a str,
    windows: Vec<WindowTotal<'a>>,
    balances: Vec<BalanceTotal<'a>>,
}

/// The summed remaining and full allowance of one window name.
#[derive(Debug, PartialEq)]
struct WindowTotal<'a> {
    name: &'a str,
    prefix: &'a str,
    suffix: &'a str,
    remaining: f64,
    total: f64,
}

/// The summed prepaid balance in one currency.
#[derive(Debug, PartialEq)]
struct BalanceTotal<'a> {
    symbol: &'a str,
    amount: f64,
}

/// Sum the quota of the given cards (provider name, reading) per provider,
/// sorted by provider name. `now` decides which windows have already reset.
pub(super) fn summarize<'a>(
    cards: impl Iterator<Item = (&'a str, &'a KeyQuotaView)>,
    now: DateTime<Utc>,
) -> Vec<ProviderTotal<'a>> {
    let mut by_provider: BTreeMap<&'a str, ProviderTotal<'a>> = BTreeMap::new();
    for (provider, quota) in cards {
        let entry = by_provider
            .entry(provider)
            .or_insert_with(|| ProviderTotal {
                provider,
                windows: Vec::new(),
                balances: Vec::new(),
            });
        for window in &quota.windows {
            let Some((prefix, total, suffix)) = window.total().and_then(parse_amount) else {
                continue;
            };
            let renewed = window.resets_at().is_some_and(|resets_at| resets_at <= now);
            let remaining = if renewed {
                total
            } else {
                total * (100. - window.used_pct()).clamp(0., 100.) / 100.
            };
            match entry.windows.iter_mut().find(|w| w.name == window.name()) {
                Some(sum) if sum.prefix == prefix && sum.suffix == suffix => {
                    sum.remaining += remaining;
                    sum.total += total;
                }
                Some(_) => {}
                None => entry.windows.push(WindowTotal {
                    name: window.name(),
                    prefix,
                    suffix,
                    remaining,
                    total,
                }),
            }
        }
        if let Some(balance) = &quota.balance {
            match entry
                .balances
                .iter_mut()
                .find(|b| b.symbol == balance.symbol())
            {
                Some(sum) => sum.amount += balance.amount(),
                None => entry.balances.push(BalanceTotal {
                    symbol: balance.symbol(),
                    amount: balance.amount(),
                }),
            }
        }
    }
    by_provider
        .into_values()
        .filter(|total| !(total.windows.is_empty() && total.balances.is_empty()))
        .collect()
}

/// An amount in the window's unit: money (a prefix or suffix is present)
/// with two decimals; a bare count (credits) rounded to an integer.
struct Amount<'a> {
    prefix: &'a str,
    value: f64,
    suffix: &'a str,
}

impl fmt::Display for Amount<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix.is_empty() && self.suffix.is_empty() {
            write!(f, "{:.0}", self.value.round())
        } else {
            let value = (self.value * 100.).round() / 100.;
            write!(f, "{}{value:.2}{}", self.prefix, self.suffix)
        }
    }
}

/// "kiro-eu-central-1 本月 Credits 152340/400000, ¥45.60".
impl fmt::Display for ProviderTotal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.provider)?;
        let mut separator = " ";
        for window in &self.windows {
            let amount = |value| Amount {
                prefix: window.prefix,
                value,
                suffix: window.suffix,
            };
            write!(
                f,
                "{separator}{} {}/{}",
                window.name,
                amount(window.remaining),
                amount(window.total)
            )?;
            separator = ", ";
        }
        for balance in &self.balances {
            write!(f, "{separator}{}{:.2}", balance.symbol, balance.amount)?;
            separator = ", ";
        }
        Ok(())
    }
}

/// The single-line summary filling the gap between the tabs and the action
/// buttons; `None` when there is nothing to sum.
pub(super) fn render(totals: &[ProviderTotal<'_>], cx: &App) -> Option<Div> {
    if totals.is_empty() {
        return None;
    }
    let parts: Vec<String> = totals.iter().map(ProviderTotal::to_string).collect();
    let text = parts.join(" · ");
    Some(
        gpui::div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_center()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(text),
    )
}
