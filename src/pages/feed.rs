//! The 动态 feed card: a page-agnostic strip of recent
//! relay attempts — in-flight ones on top, finished ones below. It reads only
//! the shared [`ActivityFeed`] entity (no ledger query, no page state), so it
//! mounts wherever a page wants it (the overview page owns it today).
//!
//! A row is either a live attempt (client dot, phase + bytes) or a settled one
//! (timestamp, status · duration · speed). In-flight attempts never sink below
//! a settled row, however long they run.

use gpui::{App, Div, Entity, Hsla, ParentElement, Styled, prelude::FluentBuilder as _, px};
use gpui_kit::component::{ActiveTheme, Theme};

use gateway::activity::{EndMetrics, EndStatus};

use crate::activity::{ActivityFeed, LiveAttempt, SettledAttempt, client_hue, named_client};

use super::card;

/// How many SETTLED rows the feed shows: the live block comes first and is
/// never trimmed, so the newest settled attempts fill what is left of this
/// budget — a live burst past it lengthens the card instead of hiding work.
/// Matches the settled ring's cap.
const FEED_LIMIT: usize = crate::activity::SETTLED_CAP;

/// One row of the 动态 feed: an in-flight attempt (elapsed clock, client dot)
/// or a finished attempt (timestamp, status · duration · speed).
pub(crate) enum FeedRow {
    Live(LiveAttempt),
    Settled(SettledAttempt),
}

/// The feed rows: in-flight attempts first (newest start first), then settled
/// attempts (newest settle first), the live block never trimmed. A live
/// attempt never sinks below a settled row, however long it runs.
pub(crate) fn feed_rows(feed: &ActivityFeed) -> Vec<FeedRow> {
    // Newest start first, ordered by the monotonic `Instant` the attempt
    // recorded — exact, unlike a wall clock derived from an elapsed reading,
    // which moves between comparisons. The borrows sort first, so each snapshot
    // is cloned once, already in order.
    let mut live: Vec<&LiveAttempt> = feed.live().collect();
    live.sort_by_key(|attempt| std::cmp::Reverse(attempt.started));
    let mut rows: Vec<FeedRow> = live
        .into_iter()
        .map(|attempt| FeedRow::Live(attempt.clone()))
        .collect();
    // Settled rows already come newest first from the ring. The cap trims the
    // settled tail only, so in-flight rows are never hidden: a burst past
    // `FEED_LIMIT` lengthens the card instead of dropping live work.
    rows.extend(
        feed.settled()
            .take(FEED_LIMIT.saturating_sub(rows.len()))
            .cloned()
            .map(FeedRow::Settled),
    );
    rows
}

/// The 动态 feed card: in-flight attempts (client dot, phase + bytes) over the
/// newest settled attempts (timestamp, status · duration · speed), most recent
/// first, the settled rows capped by [`FEED_LIMIT`]. Renders even when empty —
/// a quiet "暂无请求" line keeps the section's place in the layout.
pub(crate) fn feed_card(feed: &Entity<ActivityFeed>, cx: &App) -> Div {
    let theme = cx.theme();
    let rows = feed_rows(feed.read(cx));
    let mut body = gpui::div().flex().flex_col();
    if rows.is_empty() {
        body = body.child(
            gpui::div()
                .h(px(26.))
                .flex()
                .items_center()
                .px(px(4.))
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child("暂无请求"),
        );
    } else {
        for row in &rows {
            body = body.child(feed_row(row, cx));
        }
    }
    // The body is the card's only child — no header row.
    card(cx).p_3().child(body)
}

/// One compact line: `HH:MM:SS` · client dot+name · route · 供应商
/// (`account/model`) · reason · meta. In-flight rows show the phase + bytes,
/// settled rows the status · duration · speed — the color carries the state
/// (muted = ok, danger = error), no background blocks.
fn feed_row(row: &FeedRow, cx: &App) -> Div {
    let theme = cx.theme();
    match row {
        FeedRow::Live(attempt) => {
            let name = named_client(attempt.agent.as_deref(), attempt.caller.as_deref());
            // Full alpha: the overview page has no animation beat, so a live client's
            // dot is steady rather than breathing.
            let dot = client_hue(&name);
            let elapsed = humanize_elapsed(attempt.started.elapsed());
            let mut meta = attempt.phase().to_string();
            if attempt.streaming {
                meta = format!(
                    "{meta} · {}B",
                    crate::fmt::humanize_count(attempt.bytes as i64)
                );
            }
            feed_line(
                theme,
                FeedLine {
                    time: elapsed,
                    dot,
                    name,
                    route: route_of(&attempt.client_model, attempt.route.as_deref()),
                    landing: crate::fmt::account_model(
                        attempt.account.as_deref(),
                        Some(attempt.served_model.as_str()),
                    ),
                    reason: reason_of(&attempt.reason),
                    meta,
                    meta_color: theme.muted_foreground,
                },
            )
        }
        FeedRow::Settled(attempt) => {
            let name = named_client(attempt.agent.as_deref(), attempt.caller.as_deref());
            let dot = client_hue(&name);
            let (status, color) = match attempt.status {
                EndStatus::Ok => ("ok", theme.muted_foreground),
                EndStatus::Error => ("error", theme.danger),
            };
            let time = stamp_of(attempt.settled_at);
            // Millisecond duration, like the requests page's total.
            let duration = humanize_ms(attempt.duration.as_millis().min(i32::MAX as u128) as i32);
            feed_line(
                theme,
                FeedLine {
                    time,
                    dot,
                    name,
                    route: route_of(&attempt.client_model, attempt.route.as_deref()),
                    landing: crate::fmt::account_model(
                        attempt.account.as_deref(),
                        Some(attempt.served_model.as_str()),
                    ),
                    reason: reason_of(&attempt.reason),
                    meta: settled_meta(status, &duration, &attempt.metrics),
                    meta_color: color,
                },
            )
        }
    }
}

/// The route column: the route alias when the client hit a route, else the
/// model id the client sent (a direct call, `provider/model`).
fn route_of(client_model: &str, route: Option<&str>) -> String {
    route.unwrap_or(client_model).to_string()
}

/// The settled row's meta: `ok · 1.2s`, plus ` · 120 t/s` when the ledger
/// carries a decode window (the same [`crate::fmt::format_speed`] as the 请求
/// page's 速度 column, so the two agree) and ` · 总… 入… 出… 缓…` when it
/// carries tokens. A non-streaming or untimed attempt has no window — the
/// requests page dashes it, the feed leaves it out.
fn settled_meta(status: &str, duration: &str, metrics: &EndMetrics) -> String {
    let mut meta = format!("{status} · {duration}");
    if crate::fmt::decode_window(metrics.output_tokens, metrics.total_ms, metrics.ttft_ms).is_some()
    {
        meta.push_str(" · ");
        meta.push_str(&crate::fmt::format_speed(
            metrics.output_tokens,
            metrics.total_ms,
            metrics.ttft_ms,
        ));
    }
    if let Some(tokens) = token_group(metrics) {
        meta.push_str(" · ");
        meta.push_str(&tokens);
    }
    meta
}

/// The token group `总2.6K 入2.3K 出385 缓0` — the ledger's split, compact
/// ([`crate::fmt::humanize_count`]). 总 is `input + output`: the kernel's
/// `input_tokens` is the whole prompt with cache reads already inside it, so
/// nothing is double-counted; 缓 is that prompt's cache-read slice. None when
/// the attempt billed no tokens at all (a pre-response failure); a measured
/// zero reads `0`.
fn token_group(metrics: &EndMetrics) -> Option<String> {
    let input = metrics.input_tokens?;
    let output = metrics.output_tokens.unwrap_or(0);
    let cached = metrics.cached_input_tokens.unwrap_or(0);
    Some(format!(
        "{}/{}/{}/{}",
        crate::fmt::humanize_count(input + output),
        crate::fmt::humanize_count(input),
        crate::fmt::humanize_count(output),
        crate::fmt::humanize_count(cached),
    ))
}

/// The Smart strategy's reason for the chosen model, or None when empty (an
/// ordered route / resting candidate has none; no element renders).
fn reason_of(reason: &str) -> Option<String> {
    (!reason.is_empty()).then(|| reason.to_string())
}

/// One feed line's content (keeps feed_line under the argument limit — the
/// same consolidation as relay's AttemptStart).
struct FeedLine {
    time: String,
    dot: Hsla,
    name: String,
    route: String,
    /// The 供应商 cell: the account the attempt landed on and the upstream
    /// model it served (`cc/deepseek-v4.1-flash`) — rendered by the shared
    /// [`crate::fmt::account_model`], so the feed and the 请求 list agree.
    landing: String,
    /// The Smart strategy's reason for the chosen model (None = no reason).
    reason: Option<String>,
    meta: String,
    meta_color: Hsla,
}

/// The shared row layout for both feed row kinds.
fn feed_line(theme: &Theme, line: FeedLine) -> Div {
    gpui::div()
        .h(px(26.))
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(4.))
        .child(
            gpui::div()
                .w(px(84.))
                .flex_shrink_0()
                .truncate()
                .font_family("Menlo")
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(line.time),
        )
        .child(
            gpui::div()
                .w(px(7.))
                .h(px(7.))
                .flex_shrink_0()
                .rounded_full()
                .bg(line.dot),
        )
        .child(
            gpui::div()
                .flex_shrink_0()
                .max_w(px(96.))
                .truncate()
                .text_size(px(11.))
                .text_color(theme.foreground)
                .child(line.name),
        )
        .child(
            gpui::div()
                .flex_shrink_0()
                .max_w(px(180.))
                .truncate()
                .font_family("Menlo")
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .bg(theme.secondary)
                .rounded(px(5.))
                .px(px(5.))
                .child(line.route),
        )
        // The 供应商 cell — the account landed on and the upstream model served
        // (`cc/deepseek-v4.1-flash`) — takes the line's slack, so the reason and
        // meta sit right after it. Ellipsized, never wrapped: a namespaced id is
        // wider than the line.
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .text_color(theme.foreground)
                .child(line.landing),
        )
        // The Smart verdict reason sits between the landing and the meta: a small
        // muted aside, never folded into the landing string (which keeps flex_1).
        .when_some(line.reason, |this, reason| {
            this.child(
                gpui::div()
                    .flex_shrink_0()
                    .max_w(px(160.))
                    .truncate()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(reason),
            )
        })
        .child(
            gpui::div()
                .flex_shrink_0()
                .truncate()
                .text_size(px(11.))
                .text_color(line.meta_color)
                .child(line.meta),
        )
}

/// The row's wall-clock stamp for a settled attempt: `HH:MM:SS` today, `昨天
/// HH:MM` yesterday, `MM-DD HH:MM` older — a settled row survives in memory
/// across days, so a bare clock would make a stale row read as fresh.
fn stamp_of(at: chrono::DateTime<chrono::Utc>) -> String {
    let local = at.with_timezone(&crate::ui_tz::display_tz());
    let today = chrono::Utc::now()
        .with_timezone(&crate::ui_tz::display_tz())
        .date_naive();
    if local.date_naive() == today {
        local.format("%H:%M:%S").to_string()
    } else if local.date_naive() == today.pred_opt().unwrap_or(today) {
        format!("昨天 {}", local.format("%H:%M"))
    } else {
        local.format("%m-%d %H:%M").to_string()
    }
}

/// `3s` / `1:12` — an in-flight attempt's elapsed time.
fn humanize_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// `1.2s` / `280ms` — a settled request's total duration.
fn humanize_ms(ms: i32) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.)
    } else {
        format!("{ms}ms")
    }
}
