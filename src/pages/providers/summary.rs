//! The available-quota summary in the 账号 tab row: per provider, the summed
//! remaining allowance of every quota window that reports an absolute total,
//! plus the summed prepaid balances. Percent-only windows carry no amount and
//! are skipped.
//!
//! Derived on every render from the cards the caller admits (it decides
//! visibility and [`spends_quota`](super::pool::PoolAccount::spends_quota));
//! nothing is cached. The amount arithmetic is the [`pool`](super::pool)
//! module's, the same rule the pooled provider card uses, so for windows that
//! report an absolute total the tab row and the pooled cards agree. The scope
//! differs for percent-only windows: the pooled card shows them
//! (`剩 x.xx / n 个账号`), but they have no amount, so this summary skips them
//! and a provider with only percent-only windows and no balance gets no line
//! here. [`summarize`] is pure, so the aggregation is testable without gpui.
//!
//! Rules:
//! - Providers aggregate by provider id and render in id order: two providers
//!   may share a name, so the name is display only. A provider with neither an
//!   amount window nor a balance gets no line.
//! - Windows aggregate by window name within a provider, in first-seen order.
//!   [`pool::amount_reading`](super::pool::amount_reading) turns a reading into
//!   its amount (a passed reset counts as full, an unparseable total is
//!   skipped) and [`pool::add_amount`](super::pool::add_amount) sums it: the
//!   first reading fixes the unit, a later one in another unit is skipped.
//! - Balances aggregate by `(provider, symbol)`.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use gpui::{App, Div, ParentElement, Styled, px};
use gpui_kit::component::ActiveTheme;

use super::pool;
use super::quota::Amount;
use super::quota::KeyQuotaView;

/// One card's reading as the tab-row summary sees it: the provider it belongs
/// to and the reading itself. The provider id is the grouping key; the name is
/// what the line shows.
pub(super) struct Reading<'a> {
    pub(super) provider_id: i64,
    pub(super) provider_name: &'a str,
    pub(super) quota: &'a KeyQuotaView,
}

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
    amount: pool::PoolAmount<'a>,
}

/// The summed prepaid balance in one currency.
#[derive(Debug, PartialEq)]
struct BalanceTotal<'a> {
    symbol: &'a str,
    amount: f64,
}

/// Sum the given readings per provider, sorted by provider id. `now` decides
/// which windows have already reset.
pub(super) fn summarize<'a>(
    readings: impl Iterator<Item = Reading<'a>>,
    now: DateTime<Utc>,
) -> Vec<ProviderTotal<'a>> {
    let mut by_provider: BTreeMap<i64, ProviderTotal<'a>> = BTreeMap::new();
    for Reading {
        provider_id,
        provider_name,
        quota,
    } in readings
    {
        let entry = by_provider
            .entry(provider_id)
            .or_insert_with(|| ProviderTotal {
                provider: provider_name,
                windows: Vec::new(),
                balances: Vec::new(),
            });
        for window in &quota.windows {
            // The pooled card's amount rule, unreadable totals included.
            let Some(reading) = pool::amount_reading(window, now) else {
                continue;
            };
            match entry.windows.iter_mut().find(|w| w.name == window.name()) {
                Some(sum) => pool::add_amount(&mut sum.amount, reading),
                None => entry.windows.push(WindowTotal {
                    name: window.name(),
                    amount: reading,
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

/// "kiro-eu-central-1 本月 Credits 152340/400000, ¥45.60".
impl fmt::Display for ProviderTotal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.provider)?;
        let mut separator = " ";
        for window in &self.windows {
            let amount = |value| Amount {
                prefix: window.amount.prefix,
                value,
                suffix: window.amount.suffix,
            };
            write!(
                f,
                "{separator}{} {}/{}",
                window.name,
                amount(window.amount.remaining),
                amount(window.amount.total)
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use gateway::quota::KeyQuota;
    use gateway::quota::QuotaWindow;

    /// Float comparison for the sums: adding percentages carries binary noise.
    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
            .single()
            .expect("the test instants are valid")
    }

    fn now() -> DateTime<Utc> {
        at(2026, 11, 1)
    }

    fn window(
        name: &str,
        used_pct: f64,
        total: Option<&str>,
        resets_at: Option<DateTime<Utc>>,
    ) -> QuotaWindow {
        QuotaWindow {
            name: name.to_string(),
            used_pct,
            resets_at,
            period_minutes: Some(43200),
            total: total.map(str::to_string),
        }
    }

    fn reading(windows: Vec<QuotaWindow>) -> KeyQuotaView {
        KeyQuotaView::new(
            KeyQuota {
                windows,
                balance: None,
                details: Vec::new(),
            },
            now(),
            &[],
            &[],
        )
    }

    fn provider<'a>(provider_id: i64, name: &'a str, quota: &'a KeyQuotaView) -> Reading<'a> {
        Reading {
            provider_id,
            provider_name: name,
            quota,
        }
    }

    #[test]
    fn a_reading_in_another_unit_is_skipped() {
        let yen = reading(vec![window("本月", 50., Some("¥100"), None)]);
        let dollar = reading(vec![window("本月", 50., Some("$200"), None)]);
        let totals = summarize(
            [provider(1, "acme", &yen), provider(1, "acme", &dollar)].into_iter(),
            now(),
        );
        assert_eq!(totals.len(), 1, "one line per provider id");
        assert_eq!(totals[0].windows.len(), 1, "one row per window name");
        let amount = &totals[0].windows[0].amount;
        assert!(
            close(amount.remaining, 50.),
            "only the ¥50 remains; the $ reading cannot be added, got {}",
            amount.remaining
        );
        assert!(
            close(amount.total, 100.),
            "the ¥ total stays 100, got {}",
            amount.total
        );
    }

    #[test]
    fn a_window_whose_reset_passed_counts_as_full() {
        let renewed = reading(vec![window(
            "本月",
            80.,
            Some("¥100"),
            Some(at(2026, 10, 20)),
        )]);
        let totals = summarize([provider(1, "acme", &renewed)].into_iter(), now());
        let amount = &totals[0].windows[0].amount;
        assert!(
            close(amount.remaining, 100.),
            "the window renewed while the snapshot is stale, got {}",
            amount.remaining
        );
    }

    #[test]
    fn providers_group_by_id_so_shared_names_stay_apart() {
        let first = reading(vec![window("本月", 0., Some("$10"), None)]);
        let second = reading(vec![window("本月", 0., Some("$20"), None)]);
        let totals = summarize(
            [provider(2, "acme", &first), provider(1, "acme", &second)].into_iter(),
            now(),
        );
        assert_eq!(totals.len(), 2, "two providers may share one name");
        assert_eq!(
            totals[0].windows[0].amount.total, 20.,
            "provider id 1 sorts first"
        );
        assert_eq!(
            totals[1].windows[0].amount.total, 10.,
            "provider id 2 keeps its own line"
        );
    }

    #[test]
    fn percent_only_and_unreadable_totals_are_skipped() {
        let quota = reading(vec![
            window("本月", 40., None, None),
            window("5 小时", 40., Some("n/a"), None),
        ]);
        let totals = summarize([provider(1, "acme", &quota)].into_iter(), now());
        assert!(
            totals.is_empty(),
            "a percent-only window and an unreadable total carry no amount"
        );
    }
}
