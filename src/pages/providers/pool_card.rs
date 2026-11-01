//! Rendering of the pooled provider card's parts (全部 / per-provider
//! filters): the account count line, the pooled quota block, the variant
//! marker row and the compact per-account rows. [`ProvidersPage::provider_card`]
//! in `cards.rs` assembles them beside the per-account `key_card`; the numbers
//! come from the pure `pool` module.

use chrono::DateTime;
use chrono::Utc;
use gpui::{
    App, Context, Div, InteractiveElement, ParentElement, Stateful, Styled,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::ActiveTheme;

use gateway::store::{ProviderRow, RestRow};

use super::cards::{key_menu_button, live_rests, rest_tags, variant_markers};
use super::pool;
use super::pool::{PoolCounts, Pooled};
use super::quota::Amount;
use super::{KeyCardData, ProvidersPage};

/// One compact account row of a pooled provider card: label, the used share
/// of the account's tightest window, the shared ⋯ menu, and any cooldown
/// tags still in force at `now`. A disabled account's row is red — it takes no
/// new requests.
pub(super) fn pooled_account_row(
    card_data: &KeyCardData,
    rests: &[RestRow],
    now: DateTime<Utc>,
    cx: &mut Context<ProvidersPage>,
) -> Stateful<Div> {
    let key = &card_data.key;
    let rests = live_rests(rests, now);
    let used = match card_data
        .quota
        .as_ref()
        .and_then(|quota| pool::tightest_used_pct(quota, now))
    {
        Some(used) => format!("{used:.0}%"),
        None => "未探测".to_string(),
    };
    let (label_color, used_color) = if key.enabled {
        (cx.theme().foreground, cx.theme().muted_foreground)
    } else {
        (cx.theme().danger, cx.theme().danger)
    };
    gpui::div()
        .id(("pool-account", key.id as u64))
        .flex()
        .flex_col()
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(label_color)
                        .child(key.label.clone()),
                )
                .child(
                    gpui::div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(used_color)
                        .child(used),
                )
                .child(key_menu_button(
                    card_data.provider.id,
                    key,
                    cx.weak_entity(),
                )),
        )
        // A cooldown-free row has no footer: the row stays one line.
        .when(!rests.is_empty(), |this| {
            this.child(rest_tags(key.id, &rests, now, cx))
        })
}

/// The pooled card's count line, `可用/总数/停用`: the disabled count carries
/// the error color and disappears when no account is disabled.
pub(super) fn pool_counts(counts: PoolCounts, cx: &App) -> Div {
    let mut row = gpui::div()
        .flex_none()
        .flex()
        .items_center()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(format!("{}/{}", counts.available, counts.total));
    if counts.disabled > 0 {
        row = row.child(
            gpui::div()
                .text_color(cx.theme().danger)
                .child(format!("/{}", counts.disabled)),
        );
    }
    row
}

/// The pooled card's quota block: one row per window — its pooled remainder and
/// the remainder per reset segment below it — then one row per currency of
/// prepaid balance. `None` when no available account reported anything.
pub(super) fn pool_quota(pooled: &Pooled<'_>, cx: &App) -> Option<Div> {
    if pooled.windows.is_empty() && pooled.balances.is_empty() {
        return None;
    }
    let mut block = gpui::div().flex().flex_col().gap_1();
    for window in &pooled.windows {
        let headline = match &window.amount {
            Some(amount) => format!(
                "{} · 剩 {}/{}",
                window.name,
                Amount {
                    prefix: amount.prefix,
                    value: amount.remaining,
                    suffix: amount.suffix,
                },
                Amount {
                    prefix: amount.prefix,
                    value: amount.total,
                    suffix: amount.suffix,
                },
            ),
            None => format!(
                "{} · 剩 {:.2} / {} 个账号",
                window.name, window.remaining, window.accounts
            ),
        };
        block = block.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().foreground)
                .child(headline),
        );
        // An amount headline already says it all; a percent segment line under
        // it would mix two forms that cannot be added.
        if window.shows_segments() {
            block = block.child(
                gpui::div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(pool::segments_line(&window.segments)),
            );
        }
    }
    for balance in &pooled.balances {
        block = block.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(cx.theme().foreground)
                .child(format!("余额 {}{:.2}", balance.symbol, balance.amount)),
        );
    }
    Some(block)
}

/// The variant markers alone (区域 + Coding Plan), for the pooled provider card:
/// group tags only describe a single account. `None` when the provider is no
/// catalog variant.
pub(super) fn variant_row(provider: &ProviderRow, cx: &App) -> Option<Div> {
    let markers = variant_markers(provider, cx);
    if markers.is_empty() {
        return None;
    }
    Some(
        gpui::div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(markers),
    )
}
