//! The pooled reading of one provider's accounts: the 全部 and per-provider
//! filters show ONE summary card per provider instead of one card per account
//! (see [`GroupFilter::pooled`](super::groups::GroupFilter::pooled)).
//!
//! Derived on every render from the loaded cards; nothing is cached, so the
//! summary cannot drift from the accounts it pools. [`pool`] is pure (the
//! clock and the display offset are parameters) so the arithmetic is testable
//! without a page.
//!
//! Rules:
//! - Archived accounts are gone from the provider; a count of `available /
//!   total / disabled` covers the rest, so `total == available + disabled`.
//! - Only accounts that can still spend pool their quota (enabled, unarchived,
//!   their provider enabled): a disabled account or a disabled provider
//!   receives no new requests, so a reading there must not inflate the pool.
//! - Windows pool by name in first-seen order. A window whose reset instant
//!   has passed has renewed while its snapshot is stale, so it counts as full
//!   ([`remaining_share`]).
//! - A window reporting an absolute total sums its remaining amounts and its
//!   totals per unit ([`amount_reading`] and [`add_amount`]): a total that does
//!   not parse as a number is skipped, the first reading fixes the unit, and a
//!   later reading in a different unit cannot be added and is skipped. The
//!   tab-row summary (`summary.rs`) sums with the same two functions, so the
//!   two can never disagree.
//! - A percent-only window sums remaining shares (1.0 = one full account) and
//!   splits into reset segments: accounts renewing on the same local date
//!   merge, segments order by renewal date, and an already-renewed window and
//!   an unknown reset are each their own segment.
//! - A window that mixes absolute amounts with percent-only readings headlines
//!   only the amount: a summed amount and a summed share cannot be added, so
//!   the amount line suppresses the segments the percent accounts formed
//!   ([`PoolWindow::shows_segments`]).
//! - Prepaid balances pool by currency symbol.

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::NaiveDate;
use chrono::Utc;

use super::KeyCardData;
use super::quota::KeyQuotaView;
use super::quota::QuotaWindowView;
use super::quota::parse_amount;

/// One account as the pool sees it: the flags that decide whether it counts and
/// whether its reading may be spent, plus the reading when the page has one.
/// Borrows the reading, so a render clones nothing.
#[derive(Clone, Copy)]
pub(super) struct PoolAccount<'a> {
    pub(super) enabled: bool,
    pub(super) archived: bool,
    pub(super) provider_enabled: bool,
    pub(super) quota: Option<&'a KeyQuotaView>,
}

impl<'a> From<&'a KeyCardData> for PoolAccount<'a> {
    fn from(card: &'a KeyCardData) -> Self {
        Self {
            enabled: card.key.enabled,
            archived: card.key.archived_at.is_some(),
            provider_enabled: card.provider.enabled,
            quota: card.quota.as_ref(),
        }
    }
}

impl PoolAccount<'_> {
    /// Whether this account's reading can be spent: it is enabled, it is not
    /// archived, and its provider is enabled — the gateway sends nothing to a
    /// disabled account or through a disabled provider, so quota there is
    /// unreachable. The pooled card and the tab-row summary share this one
    /// rule, so the two can never disagree.
    #[must_use]
    pub(super) fn spends_quota(&self) -> bool {
        self.enabled && !self.archived && self.provider_enabled
    }
}

/// How many of a provider's accounts count at all (unarchived) and how many of
/// those are available (enabled). `total == available + disabled`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PoolCounts {
    pub(super) available: usize,
    pub(super) total: usize,
    pub(super) disabled: usize,
}

impl PoolCounts {
    /// Count `accounts`, skipping archived ones: an archived account is gone
    /// from the provider, so it is in no count.
    #[must_use]
    pub(super) fn of(accounts: &[PoolAccount<'_>]) -> Self {
        let mut counts = Self::default();
        for account in accounts {
            if account.archived {
                continue;
            }
            counts.total += 1;
            if account.enabled {
                counts.available += 1;
            } else {
                counts.disabled += 1;
            }
        }
        counts
    }
}

/// One provider's pooled reading: its counts, windows in first-seen order, then
/// its balances in first-seen order. The counts always describe the provider;
/// `windows` and `balances` stay empty when no account can spend quota (a
/// disabled provider, only disabled accounts) or when nothing was probed.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Pooled<'a> {
    pub(super) counts: PoolCounts,
    pub(super) windows: Vec<PoolWindow<'a>>,
    pub(super) balances: Vec<PoolBalance<'a>>,
}

/// One quota window pooled across a provider's available accounts.
#[derive(Debug, PartialEq)]
pub(super) struct PoolWindow<'a> {
    /// Display name from the probe ("本月", "5 小时").
    pub(super) name: &'a str,
    /// Summed remaining share of the percent-only accounts (1.0 = one full
    /// account); `accounts` says how many accounts that is.
    pub(super) remaining: f64,
    pub(super) accounts: usize,
    /// The summed absolute form, when the accounts report a total.
    pub(super) amount: Option<PoolAmount<'a>>,
    /// Reset segments of the percent-only accounts, earliest renewal first.
    pub(super) segments: Vec<PoolSegment>,
}

impl<'a> PoolWindow<'a> {
    /// Whether the window renders its reset segments: only a percent-only
    /// window can, because a summed amount and a summed share cannot be added
    /// and one line cannot say both.
    #[must_use]
    pub(super) fn shows_segments(&self) -> bool {
        self.amount.is_none() && !self.segments.is_empty()
    }

    fn new(name: &'a str) -> Self {
        Self {
            name,
            remaining: 0.,
            accounts: 0,
            amount: None,
            segments: Vec::new(),
        }
    }
}

/// A window's summed remaining and full allowance in its unit (`"$"` …
/// `""`), rendered as "remaining/total".
#[derive(Debug, PartialEq)]
pub(super) struct PoolAmount<'a> {
    pub(super) prefix: &'a str,
    pub(super) suffix: &'a str,
    pub(super) remaining: f64,
    pub(super) total: f64,
}

/// The accounts of one percent-only window that renew at the same time.
#[derive(Debug, PartialEq)]
pub(super) struct PoolSegment {
    pub(super) reset: SegmentReset,
    /// Summed remaining share of this segment's accounts.
    pub(super) remaining: f64,
    pub(super) accounts: usize,
}

/// When a segment's accounts renew. Ordered the way the segments render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SegmentReset {
    /// These accounts renew on this local date.
    Day(NaiveDate),
    /// The reading's reset instant already passed: the window has renewed,
    /// and the next reset is unknown.
    Renewed,
    /// The vendor reports no reset instant.
    Unknown,
}

impl SegmentReset {
    /// The segment `resets_at` falls in, dated in the display offset `tz`.
    fn of(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>, tz: FixedOffset) -> Self {
        match resets_at {
            Some(resets_at) if resets_at <= now => Self::Renewed,
            Some(resets_at) => Self::Day(resets_at.with_timezone(&tz).date_naive()),
            None => Self::Unknown,
        }
    }
}

/// The pooled prepaid balance in one currency.
#[derive(Debug, PartialEq)]
pub(super) struct PoolBalance<'a> {
    pub(super) symbol: &'a str,
    pub(super) amount: f64,
}

/// Pool `accounts` into one provider's summary reading. `now` decides which
/// windows have already renewed, `tz` the calendar date a reset instant falls
/// on (accounts renewing on the same date merge into one segment). Every
/// account the provider has feeds the counts; only the accounts that can spend
/// feed the windows and balances.
#[must_use]
pub(super) fn pool<'a>(
    accounts: &[PoolAccount<'a>],
    now: DateTime<Utc>,
    tz: FixedOffset,
) -> Pooled<'a> {
    let mut pooled = Pooled {
        counts: PoolCounts::of(accounts),
        ..Pooled::default()
    };
    for account in accounts {
        if !account.spends_quota() {
            continue;
        }
        let Some(quota) = account.quota else {
            continue;
        };
        for window in &quota.windows {
            // No total at all: the window is percent-only and pools as a share.
            // A total that is not a number cannot be added to anything, so the
            // reading is skipped — the decision the tab-row summary shares.
            let amount = match window.total() {
                None => None,
                Some(_) => match amount_reading(window, now) {
                    Some(reading) => Some(reading),
                    None => continue,
                },
            };
            let share = remaining_share(window, now);
            let at = match pooled
                .windows
                .iter()
                .position(|entry| entry.name == window.name())
            {
                Some(at) => at,
                None => {
                    pooled.windows.push(PoolWindow::new(window.name()));
                    pooled.windows.len() - 1
                }
            };
            let entry = &mut pooled.windows[at];
            match amount {
                Some(reading) => match entry.amount.as_mut() {
                    Some(sum) => add_amount(sum, reading),
                    None => entry.amount = Some(reading),
                },
                None => add_share(entry, window.resets_at(), share, now, tz),
            }
        }
        if let Some(balance) = &quota.balance {
            match pooled
                .balances
                .iter_mut()
                .find(|sum| sum.symbol == balance.symbol())
            {
                Some(sum) => sum.amount += balance.amount(),
                None => pooled.balances.push(PoolBalance {
                    symbol: balance.symbol(),
                    amount: balance.amount(),
                }),
            }
        }
    }
    for window in &mut pooled.windows {
        window.segments.sort_by_key(|segment| segment.reset);
    }
    pooled
}

/// The share of `window` still available, 0–1. A window whose reset already
/// passed counts as full: it renewed, the snapshot is just stale.
fn remaining_share(window: &QuotaWindowView, now: DateTime<Utc>) -> f64 {
    if window.resets_at().is_some_and(|resets_at| resets_at <= now) {
        return 1.;
    }
    1. - window.used_pct().clamp(0., 100.) / 100.
}

/// The absolute-total reading of `window` at `now`, ready to add: `None` when
/// the window reports no total (it is percent-only) or a total that is not a
/// number — neither can be added to an amount sum. A window whose reset already
/// passed has renewed while its snapshot is stale, so its reading counts as
/// full.
#[must_use]
pub(super) fn amount_reading<'w>(
    window: &'w QuotaWindowView,
    now: DateTime<Utc>,
) -> Option<PoolAmount<'w>> {
    let (prefix, total, suffix) = window.total().and_then(parse_amount)?;
    Some(PoolAmount {
        prefix,
        suffix,
        remaining: total * remaining_share(window, now),
        total,
    })
}

/// Add one absolute-total reading to a running amount sum. The sum's unit is
/// fixed by its first reading; a reading in another unit cannot be added to it
/// and is skipped.
pub(super) fn add_amount<'a>(sum: &mut PoolAmount<'a>, reading: PoolAmount<'a>) {
    if sum.prefix == reading.prefix && sum.suffix == reading.suffix {
        sum.remaining += reading.remaining;
        sum.total += reading.total;
    }
}

/// Add one percent-only reading to a window's share pool and its reset
/// segment.
fn add_share(
    window: &mut PoolWindow<'_>,
    resets_at: Option<DateTime<Utc>>,
    share: f64,
    now: DateTime<Utc>,
    tz: FixedOffset,
) {
    window.remaining += share;
    window.accounts += 1;
    let reset = SegmentReset::of(resets_at, now, tz);
    match window
        .segments
        .iter_mut()
        .find(|segment| segment.reset == reset)
    {
        Some(segment) => {
            segment.remaining += share;
            segment.accounts += 1;
        }
        None => window.segments.push(PoolSegment {
            reset,
            remaining: share,
            accounts: 1,
        }),
    }
}

/// One line of reset segments, `11-12 前 34% · 11-23 前 71%`. A segment's
/// percentage is its pooled remainder over its accounts (a segment always has
/// at least one account): the accounts renew together, so the segment is as
/// full as its accounts average.
#[must_use]
pub(super) fn segments_line(segments: &[PoolSegment]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for segment in segments {
        let share = segment.remaining / segment.accounts as f64 * 100.;
        parts.push(match segment.reset {
            SegmentReset::Day(date) => format!("{} 前 {share:.0}%", date.format("%m-%d")),
            SegmentReset::Renewed => format!("已重置 {share:.0}%"),
            SegmentReset::Unknown => format!("重置未知 {share:.0}%"),
        });
    }
    parts.join(" · ")
}

/// The used share of the account's tightest window (`None` when it reports no
/// window at all): the live window that will block first. A window whose reset
/// already passed has renewed and is no constraint; an account whose windows
/// all renewed is back to a full allowance, i.e. `0.` used — not unprobed.
#[must_use]
pub(super) fn tightest_used_pct(quota: &KeyQuotaView, now: DateTime<Utc>) -> Option<f64> {
    if quota.windows.is_empty() {
        return None;
    }
    let live = quota
        .windows
        .iter()
        .filter(|window| !window.resets_at().is_some_and(|resets_at| resets_at <= now))
        .map(|window| window.used_pct().clamp(0., 100.));
    Some(live.reduce(f64::max).unwrap_or(0.))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use gateway::quota::Balance;
    use gateway::quota::KeyQuota;
    use gateway::quota::QuotaWindow;

    /// Float comparison for the sums: adding percentages carries binary noise.
    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("the test instants are valid")
    }

    fn now() -> DateTime<Utc> {
        at(2026, 11, 1, 0)
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).expect("UTC is a valid offset")
    }

    /// A percent-only window (no absolute total).
    fn pct(name: &str, used_pct: f64, resets_at: Option<DateTime<Utc>>) -> QuotaWindow {
        QuotaWindow {
            name: name.to_string(),
            used_pct,
            resets_at,
            period_minutes: Some(43200),
            total: None,
        }
    }

    /// A money window with an absolute total.
    fn money(name: &str, used_pct: f64, total: &str) -> QuotaWindow {
        QuotaWindow {
            total: Some(total.to_string()),
            ..pct(name, used_pct, None)
        }
    }

    fn reading(windows: Vec<QuotaWindow>, balance: Option<Balance>) -> KeyQuotaView {
        KeyQuotaView::new(
            KeyQuota {
                windows,
                balance,
                details: Vec::new(),
            },
            now(),
            &[],
            &[],
        )
    }

    fn account_of(
        enabled: bool,
        archived: bool,
        provider_enabled: bool,
        quota: Option<&KeyQuotaView>,
    ) -> PoolAccount<'_> {
        PoolAccount {
            enabled,
            archived,
            provider_enabled,
            quota,
        }
    }

    /// An account on an enabled provider (the common case).
    fn account(enabled: bool, archived: bool, quota: Option<&KeyQuotaView>) -> PoolAccount<'_> {
        account_of(enabled, archived, true, quota)
    }

    /// An enabled account whose provider is disabled: no new requests, so its
    /// reading can never be spent.
    fn account_of_disabled_provider(quota: Option<&KeyQuotaView>) -> PoolAccount<'_> {
        account_of(true, false, false, quota)
    }

    #[test]
    fn counts_skip_archived_and_count_disabled() {
        let accounts = [
            account(true, false, None),
            account(true, false, None),
            account(false, false, None),
            account(true, true, None),
            account(false, true, None),
        ];
        let counts = PoolCounts::of(&accounts);
        assert_eq!(
            counts,
            PoolCounts {
                available: 2,
                total: 3,
                disabled: 1
            },
            "archived accounts 4 and 5 leave the provider entirely; account 3 is disabled"
        );
    }

    #[test]
    fn same_reset_date_merges_and_dates_order_the_segments() {
        // Inserted newest-first: the segments must still come out in date order.
        let late = reading(vec![pct("本月", 29., Some(at(2026, 11, 23, 5)))], None);
        let early_a = reading(vec![pct("本月", 20., Some(at(2026, 11, 12, 3)))], None);
        let early_b = reading(vec![pct("本月", 48., Some(at(2026, 11, 12, 15)))], None);
        let accounts = [
            account(true, false, Some(&late)),
            account(true, false, Some(&early_a)),
            account(true, false, Some(&early_b)),
        ];
        let pooled = pool(&accounts, now(), utc());
        assert_eq!(
            pooled.windows.len(),
            1,
            "one window name pools into one row"
        );
        let window = &pooled.windows[0];
        assert_eq!(window.name, "本月");
        assert_eq!(window.accounts, 3, "all three accounts report the window");
        assert!(
            close(window.remaining, 2.03),
            "remaining shares sum to 2.03, got {}",
            window.remaining
        );
        let segments: Vec<(SegmentReset, usize)> = window
            .segments
            .iter()
            .map(|segment| (segment.reset, segment.accounts))
            .collect();
        assert_eq!(
            segments,
            [
                (
                    SegmentReset::Day(NaiveDate::from_ymd_opt(2026, 11, 12).expect("valid date")),
                    2
                ),
                (
                    SegmentReset::Day(NaiveDate::from_ymd_opt(2026, 11, 23).expect("valid date")),
                    1
                ),
            ],
            "accounts renewing on the same date share a segment; dates order"
        );
        assert!(
            close(window.segments[0].remaining, 1.32),
            "the 11-12 segment sums 0.80 + 0.52, got {}",
            window.segments[0].remaining
        );
        assert!(
            window.amount.is_none(),
            "a percent-only window has no amount"
        );
        assert!(
            window.shows_segments(),
            "a percent-only window renders its reset segments"
        );
    }

    #[test]
    fn unknown_reset_is_its_own_last_segment() {
        let dated = reading(vec![pct("本月", 40., Some(at(2026, 11, 12, 3)))], None);
        let undated = reading(vec![pct("本月", 10., None)], None);
        let accounts = [
            account(true, false, Some(&undated)),
            account(true, false, Some(&dated)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let resets: Vec<SegmentReset> = pooled.windows[0]
            .segments
            .iter()
            .map(|segment| segment.reset)
            .collect();
        assert_eq!(
            resets,
            [
                SegmentReset::Day(NaiveDate::from_ymd_opt(2026, 11, 12).expect("valid date")),
                SegmentReset::Unknown,
            ],
            "an unknown reset cannot be dated, so it renders after the dated ones"
        );
    }

    #[test]
    fn dated_segments_come_before_renewed_and_undated_ones() {
        // Fed newest-first and mixed: a dated window, a window whose reset
        // passed (renewed) and a window without a reset instant.
        let renewed = reading(vec![pct("本月", 95., Some(at(2026, 10, 31, 6)))], None);
        let undated = reading(vec![pct("本月", 10., None)], None);
        let dated = reading(vec![pct("本月", 40., Some(at(2026, 11, 12, 3)))], None);
        let accounts = [
            account(true, false, Some(&renewed)),
            account(true, false, Some(&undated)),
            account(true, false, Some(&dated)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let resets: Vec<SegmentReset> = pooled.windows[0]
            .segments
            .iter()
            .map(|segment| segment.reset)
            .collect();
        assert_eq!(
            resets,
            [
                SegmentReset::Day(NaiveDate::from_ymd_opt(2026, 11, 12).expect("valid date")),
                SegmentReset::Renewed,
                SegmentReset::Unknown,
            ],
            "a dated renewal reads first, then an already-renewed window, then the undated one"
        );
    }

    #[test]
    fn passed_reset_counts_as_full() {
        let stale = reading(vec![pct("本月", 95., Some(at(2026, 10, 31, 6)))], None);
        let accounts = [account(true, false, Some(&stale))];
        let pooled = pool(&accounts, now(), utc());
        let window = &pooled.windows[0];
        assert!(
            close(window.remaining, 1.),
            "a window past its reset renewed: it counts as full, got {}",
            window.remaining
        );
        assert_eq!(
            window.segments[0].reset,
            SegmentReset::Renewed,
            "the stale reset is no renewal date"
        );
    }

    #[test]
    fn disabled_accounts_stay_out_of_the_pool() {
        let spent = reading(vec![pct("本月", 100., Some(at(2026, 11, 12, 3)))], None);
        let untouched = reading(vec![pct("本月", 0., Some(at(2026, 11, 12, 3)))], None);
        let accounts = [
            account(true, false, Some(&spent)),
            account(false, false, Some(&untouched)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let window = &pooled.windows[0];
        assert_eq!(
            window.accounts, 1,
            "the disabled account's reading is not pooled"
        );
        assert!(
            close(window.remaining, 0.),
            "only the spent account counts, got {}",
            window.remaining
        );
        assert_eq!(
            pooled.counts,
            PoolCounts {
                available: 1,
                total: 2,
                disabled: 1
            }
        );
    }

    #[test]
    fn absolute_totals_sum_as_amounts() {
        let first = reading(vec![money("Credits", 20., "10000")], None);
        let second = reading(vec![money("Credits", 50., "10000")], None);
        let accounts = [
            account(true, false, Some(&first)),
            account(true, false, Some(&second)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let PoolAmount {
            prefix,
            suffix,
            remaining,
            total,
        } = pooled.windows[0].amount.as_ref().expect("an amount window");
        assert_eq!((*prefix, *suffix), ("", ""), "a bare count carries no unit");
        assert!(close(*total, 20000.), "totals sum, got {total}");
        assert!(
            close(*remaining, 13000.),
            "remaining sums 8000 + 5000, got {remaining}"
        );
        assert_eq!(
            pooled.windows[0].accounts, 0,
            "an amount window is not a percent-only account pool"
        );
    }

    #[test]
    fn balances_sum_per_symbol() {
        let yuan = |amount: f64| {
            reading(
                Vec::new(),
                Some(Balance {
                    amount,
                    symbol: "¥".to_string(),
                }),
            )
        };
        let dollars = reading(
            Vec::new(),
            Some(Balance {
                amount: 3.5,
                symbol: "$".to_string(),
            }),
        );
        let ten = yuan(10.);
        let two_fifty = yuan(2.5);
        let accounts = [
            account(true, false, Some(&ten)),
            account(true, false, Some(&dollars)),
            account(true, false, Some(&two_fifty)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let balances: Vec<(&str, f64)> = pooled
            .balances
            .iter()
            .map(|balance| (balance.symbol, balance.amount))
            .collect();
        assert_eq!(balances.len(), 2, "one row per currency");
        assert_eq!(balances[0].0, "¥", "first seen first");
        assert!(
            close(balances[0].1, 12.5),
            "yuan sums, got {}",
            balances[0].1
        );
        assert!(close(balances[1].1, 3.5), "dollars stay separate");
    }

    #[test]
    fn provider_without_accounts_pools_to_nothing() {
        let pooled = pool(&[], now(), utc());
        assert_eq!(
            pooled.counts,
            PoolCounts::default(),
            "no accounts, no counts"
        );
        assert!(pooled.windows.is_empty(), "no account reports a window");
        assert!(pooled.balances.is_empty(), "no account reports a balance");
    }

    #[test]
    fn segments_line_reads_each_segments_average_remainder() {
        let segments = [
            PoolSegment {
                reset: SegmentReset::Day(
                    NaiveDate::from_ymd_opt(2026, 11, 12).expect("valid date"),
                ),
                remaining: 0.68,
                accounts: 2,
            },
            PoolSegment {
                reset: SegmentReset::Day(
                    NaiveDate::from_ymd_opt(2026, 11, 23).expect("valid date"),
                ),
                remaining: 0.71,
                accounts: 1,
            },
            PoolSegment {
                reset: SegmentReset::Renewed,
                remaining: 1.,
                accounts: 1,
            },
            PoolSegment {
                reset: SegmentReset::Unknown,
                remaining: 0.5,
                accounts: 1,
            },
        ];
        assert_eq!(
            segments_line(&segments),
            "11-12 前 34% · 11-23 前 71% · 已重置 100% · 重置未知 50%",
            "two accounts at 0.68 combined average 34%"
        );
    }

    #[test]
    fn tightest_used_pct_ignores_renewed_windows() {
        let view = reading(
            vec![
                pct("5 小时", 80., Some(at(2026, 11, 1, 6))),
                pct("本月", 20., Some(at(2026, 11, 20, 6))),
                pct("本周", 95., Some(at(2026, 10, 30, 6))),
            ],
            None,
        );
        let tightest = tightest_used_pct(&view, now());
        assert!(
            tightest.is_some_and(|used| close(used, 80.)),
            "the renewed 95% window is no constraint, got {tightest:?}"
        );
        assert_eq!(
            tightest_used_pct(&reading(Vec::new(), None), now()),
            None,
            "no windows, no reading"
        );
    }

    #[test]
    fn tightest_used_pct_of_an_all_renewed_account_is_zero() {
        let renewed = reading(
            vec![
                pct("本月", 95., Some(at(2026, 10, 30, 6))),
                pct("5 小时", 80., Some(at(2026, 10, 31, 20))),
            ],
            None,
        );
        assert_eq!(
            tightest_used_pct(&renewed, now()),
            Some(0.),
            "every window renewed, so the account has its full allowance back — not 未探测"
        );
        assert_eq!(
            tightest_used_pct(&reading(Vec::new(), None), now()),
            None,
            "a reading without any window is the only 未探测 case"
        );
    }

    #[test]
    fn a_disabled_provider_pools_no_quota() {
        let fresh = reading(vec![pct("本月", 0., Some(at(2026, 11, 12, 3)))], None);
        let wallet = reading(
            Vec::new(),
            Some(Balance {
                amount: 5.,
                symbol: "¥".to_string(),
            }),
        );
        let accounts = [
            account_of_disabled_provider(Some(&fresh)),
            account_of_disabled_provider(Some(&wallet)),
        ];
        let pooled = pool(&accounts, now(), utc());
        assert_eq!(
            pooled.counts,
            PoolCounts {
                available: 2,
                total: 2,
                disabled: 0
            },
            "counts stay per-account: the accounts are enabled, their provider is not"
        );
        assert!(
            pooled.windows.is_empty(),
            "a disabled provider receives no requests, so its quota cannot be spent"
        );
        assert!(
            pooled.balances.is_empty(),
            "its prepaid balance cannot be spent either"
        );
    }

    #[test]
    fn spends_quota_needs_an_enabled_account_on_an_enabled_provider() {
        let cases = [
            (true, false, true, true),
            (false, false, true, false),
            (true, true, true, false),
            (true, false, false, false),
        ];
        for (enabled, archived, provider_enabled, expected) in cases {
            let account = account_of(enabled, archived, provider_enabled, None);
            assert_eq!(
                account.spends_quota(),
                expected,
                "enabled={enabled} archived={archived} provider_enabled={provider_enabled}"
            );
        }
    }

    #[test]
    fn an_archived_account_with_a_reading_stays_out() {
        let fresh = reading(vec![pct("本月", 0., Some(at(2026, 11, 12, 3)))], None);
        let accounts = [account(true, true, Some(&fresh))];
        let pooled = pool(&accounts, now(), utc());
        assert_eq!(
            pooled.counts,
            PoolCounts::default(),
            "an archived account is gone from the provider, so it is in no count"
        );
        assert!(pooled.windows.is_empty(), "and its reading cannot be spent");
    }

    #[test]
    fn an_unparseable_total_is_skipped() {
        let broken = reading(vec![money("Credits", 20., "N/A")], None);
        let accounts = [account(true, false, Some(&broken))];
        let pooled = pool(&accounts, now(), utc());
        assert!(
            pooled.windows.is_empty(),
            "a total that is not a number is skipped, exactly as the tab-row summary skips it"
        );
    }

    #[test]
    fn a_reading_in_another_unit_is_skipped() {
        let dollars = reading(vec![money("Credits", 20., "$100")], None);
        let yuan = reading(vec![money("Credits", 20., "¥100")], None);
        let accounts = [
            account(true, false, Some(&dollars)),
            account(true, false, Some(&yuan)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let amount = pooled.windows[0]
            .amount
            .as_ref()
            .expect("the first reading fixes the unit");
        assert_eq!(amount.prefix, "$", "the first unit wins");
        assert!(
            close(amount.total, 100.),
            "the ¥ reading cannot be added to $, got {}",
            amount.total
        );
    }

    #[test]
    fn windows_keep_the_order_they_were_first_seen() {
        let first = reading(
            vec![
                pct("本月", 10., Some(at(2026, 11, 12, 3))),
                pct("5 小时", 10., Some(at(2026, 11, 1, 5))),
            ],
            None,
        );
        let second = reading(vec![pct("5 小时", 20., Some(at(2026, 11, 1, 5)))], None);
        let accounts = [
            account(true, false, Some(&first)),
            account(true, false, Some(&second)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let names: Vec<&str> = pooled.windows.iter().map(|window| window.name).collect();
        assert_eq!(
            names,
            ["本月", "5 小时"],
            "the provider's own window order, never alphabetical"
        );
        assert!(
            close(pooled.windows[1].remaining, 1.7),
            "the second account adds to the row already seen (0.9 + 0.8), got {}",
            pooled.windows[1].remaining
        );
    }

    #[test]
    fn an_amount_reading_whose_reset_passed_is_full() {
        let quota = reading(
            vec![QuotaWindow {
                resets_at: Some(at(2026, 10, 20, 0)),
                ..money("本月", 80., "$50")
            }],
            None,
        );
        let amount = amount_reading(&quota.windows[0], now()).expect("$50 parses as an amount");
        assert!(
            close(amount.remaining, 50.),
            "the window renewed while the snapshot is stale, got {}",
            amount.remaining
        );
    }

    #[test]
    fn add_amount_skips_a_reading_in_another_unit() {
        let quota = reading(
            vec![money("本月", 50., "$100"), money("本月", 0., "¥80")],
            None,
        );
        let mut sum = amount_reading(&quota.windows[0], now()).expect("$100 parses");
        let other = amount_reading(&quota.windows[1], now()).expect("¥80 parses");
        add_amount(&mut sum, other);
        assert!(
            close(sum.remaining, 50.) && close(sum.total, 100.),
            "the $ sum ignores the ¥ reading, got {}/{}",
            sum.remaining,
            sum.total
        );
    }

    #[test]
    fn archived_accounts_change_nothing_in_the_pool() {
        // The pooled card pools the provider's visible (unarchived) cards only;
        // this pins that dropping the archived ones yields the same reading.
        let quota = reading(vec![money("本月", 50., "$100")], None);
        let with_archived = [
            account(true, false, Some(&quota)),
            account(false, false, Some(&quota)),
            account(true, true, Some(&quota)),
        ];
        let unarchived = [
            account(true, false, Some(&quota)),
            account(false, false, Some(&quota)),
        ];
        assert_eq!(
            pool(&with_archived, now(), utc()),
            pool(&unarchived, now(), utc()),
            "an archived account is in no count and spends nothing"
        );
    }

    #[test]
    fn a_mixed_window_headlines_only_the_amount() {
        let absolute = reading(vec![money("Credits", 20., "10000")], None);
        let percent = reading(vec![pct("Credits", 40., Some(at(2026, 11, 12, 3)))], None);
        let accounts = [
            account(true, false, Some(&absolute)),
            account(true, false, Some(&percent)),
        ];
        let pooled = pool(&accounts, now(), utc());
        let window = &pooled.windows[0];
        assert!(
            window.amount.is_some(),
            "the absolute reading fixes the amount"
        );
        assert_eq!(
            window.segments.len(),
            1,
            "the percent reading still records its reset segment"
        );
        assert!(
            !window.shows_segments(),
            "a mixed window headlines only the amount: the two forms cannot be added"
        );
    }
}
