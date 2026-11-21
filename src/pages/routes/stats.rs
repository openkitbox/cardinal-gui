//! Pure derivations for the routes page: summary counts, request-window
//! metrics, route distribution and relative weights. No GPUI, IO or global
//! state — every value is recomputed from the caller's borrowed snapshot.

use jiff::Timestamp;

use gateway::store::{RequestRow, RouteEntry, RouteRow};

/// Counts shown in the routes summary bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RouteTotals {
    /// Number of route groups.
    pub(super) groups: usize,
    /// Number of entries across all groups.
    pub(super) entries: usize,
    /// Number of entries the operator paused.
    pub(super) paused: usize,
}

/// Derive the group/entry/paused counts from the current route snapshot.
pub(super) fn route_totals(routes: &[RouteRow]) -> RouteTotals {
    let entries: usize = routes.iter().map(|route| route.entries.len()).sum();
    let paused = routes
        .iter()
        .flat_map(|route| route.entries.iter())
        .filter(|entry| entry.paused)
        .count();
    RouteTotals {
        groups: routes.len(),
        entries,
        paused,
    }
}

/// Borrowed rows in the half-open window `(now - window, now]`: rows older
/// than the window or in the future are ignored. Every window metric iterates
/// this one helper so the view's counts and mean cannot drift apart.
pub(super) fn window_requests(
    requests: &[RequestRow],
    now: Timestamp,
    window: std::time::Duration,
) -> impl Iterator<Item = &RequestRow> {
    requests.iter().filter(move |request| {
        let age: Result<std::time::Duration, _> = now.duration_since(request.ts).try_into();
        age.is_ok_and(|age| age < window)
    })
}

/// Count requests in the window shared by [`window_requests`].
pub(super) fn window_count(
    requests: &[RequestRow],
    now: Timestamp,
    window: std::time::Duration,
) -> usize {
    window_requests(requests, now, window).count()
}

/// Mean of the recorded total latencies in seconds within the window shared
/// by [`window_requests`], or `None` when none of those rows has a sample.
pub(super) fn window_average_seconds(
    requests: &[RequestRow],
    now: Timestamp,
    window: std::time::Duration,
) -> Option<f64> {
    let (total_ms, samples) =
        window_requests(requests, now, window).fold((0_i64, 0_usize), |(total, count), request| {
            match request.total_ms {
                Some(ms) => (total.saturating_add(i64::from(ms)), count + 1),
                None => (total, count),
            }
        });
    if samples == 0 {
        return None;
    }
    Some(total_ms as f64 / samples as f64 / 1_000.0)
}

/// Route aliases and their entry counts, in route order.
pub(super) fn route_distribution(routes: &[RouteRow]) -> Vec<(&str, usize)> {
    routes
        .iter()
        .map(|route| (route.alias.as_str(), route.entries.len()))
        .collect()
}

/// Rounded integer percentage of each entry's weight relative to the total;
/// all zeros when the total is zero. Rounding is half-up (1/8 → 13%), so the
/// results need not add up to 100.
pub(super) fn weighted_shares(entries: &[RouteEntry]) -> Vec<u16> {
    let total: u128 = entries.iter().map(|entry| u128::from(entry.weight)).sum();
    if total == 0 {
        return vec![0; entries.len()];
    }
    entries
        .iter()
        .map(|entry| {
            let doubled_percent = u128::from(entry.weight) * 200 + total;
            let percent = doubled_percent / (total * 2);
            u16::try_from(percent).unwrap_or(u16::MAX)
        })
        .collect()
}
