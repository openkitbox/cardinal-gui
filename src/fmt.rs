//! Pure display helpers shared by the dashboard pages. None of these touch
//! the database; every one is unit-tested here.

/// Mask an API key for display: keep the scheme prefix up to the first `-`,
/// then an ellipsis and the last 4 characters (`sk-…wxyz`). Keys too short
/// to keep both parts lose the prefix so nothing is shown twice.
pub(crate) fn mask_api_key(key: &str) -> String {
    let tail_start = key
        .char_indices()
        .nth(key.chars().count().saturating_sub(4))
        .map_or(0, |(index, _)| index);
    let tail = &key[tail_start..];
    match key.find('-') {
        // The prefix only survives when it cannot overlap the tail.
        Some(index) if index + 1 < tail_start => format!("{}…{tail}", &key[..=index]),
        _ => format!("…{tail}"),
    }
}

/// Mask an email for display, retaining two local characters, the first domain
/// character and the domain suffix (`jo***@q***.com`).
pub(crate) fn mask_email(email: &str) -> String {
    let (local, domain) = match email.split_once('@') {
        Some((local, domain)) => (local, Some(domain)),
        None => (email, None),
    };
    let head: String = local.chars().take(2).collect();
    match domain {
        Some(domain) => {
            let (name, suffix) = domain.rsplit_once('.').unwrap_or((domain, ""));
            let first: String = name.chars().take(1).collect();
            if suffix.is_empty() {
                format!("{head}***@{first}***")
            } else {
                format!("{head}***@{first}***.{suffix}")
            }
        }
        None => format!("{head}***"),
    }
}

/// Humanize a token/request count: `406200` → `406.2k`, `1230000` → `1.2M`,
/// `3400000000` → `3.4B`.
pub(crate) fn humanize_count(n: i64) -> String {
    let abs = n.unsigned_abs();
    if abs >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if abs >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if abs >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format one request's cost (`$` with 4 decimals; a nonzero cost below display
/// precision shows `<$0.0001` instead of rounding down to zero).
pub(crate) fn format_cost(cost_usd: Option<f64>) -> String {
    match cost_usd {
        None => "—".to_string(),
        Some(cost) if cost > 0.0 && cost < 0.0001 => "<$0.0001".to_string(),
        Some(cost) => format!("${cost:.4}"),
    }
}

/// Format one request's time to first content (`842ms` / `1.2s`; None or
/// zero = not timed — non-streaming or contentless — renders as —).
pub(crate) fn format_ttft(ttft_ms: Option<i64>) -> String {
    match ttft_ms {
        Some(ms) if ms > 0 => {
            if ms < 1000 {
                format!("{ms}ms")
            } else {
                format!("{:.1}s", ms as f64 / 1000.0)
            }
        }
        _ => "—".to_string(),
    }
}

/// Decode-window guards: a window shorter than
/// 100ms tells nothing, and a rate above 10000 t/s is a vendor burst, not
/// a speed. `decode_window` returns the ms the output was written over;
/// `decode_speed` the tokens/s. The window runs from the first content
/// (text/reasoning/tool-call) — cardinal's ttft includes reasoning, and so
/// do its output tokens, so the ratio is self-consistent without a separate
/// reasoning split.
pub(crate) fn decode_window(
    output_tokens: Option<i64>,
    total_ms: Option<i32>,
    ttft_ms: Option<i64>,
) -> Option<i64> {
    const MIN_DECODE_MS: i64 = 100;
    const MAX_DECODE_SPEED: i64 = 10_000;
    let out = output_tokens?;
    let total = i64::from(total_ms?);
    let ttft = ttft_ms?;
    if out <= 0 || ttft <= 0 {
        return None;
    }
    let window = total - ttft;
    if window < MIN_DECODE_MS || out > MAX_DECODE_SPEED * window / 1000 {
        return None;
    }
    Some(window)
}

/// Format one streamed request's speed (`36 t/s`; no window = —).
pub(crate) fn format_speed(
    output_tokens: Option<i64>,
    total_ms: Option<i32>,
    ttft_ms: Option<i64>,
) -> String {
    match decode_window(output_tokens, total_ms, ttft_ms) {
        Some(window) => format!(
            "{:.0} t/s",
            output_tokens.unwrap_or(0) as f64 / (window as f64 / 1000.0)
        ),
        None => "—".to_string(),
    }
}

/// An upstream model id's vendor namespace: the leading `namespace/` of an id
/// like `deepseek/deepseek-v4.1-flash` is the vendor's own prefix, not part of
/// the model's identity (the model is `deepseek-v4.1-flash`) — the same rule
/// the gateway's 模型 trend split applies in SQL. Borrowed, untouched when the
/// id has no leading segment (no slash, or one at index 0).
pub(crate) fn strip_model_namespace(served_model: &str) -> &str {
    match served_model.split_once('/') {
        Some((namespace, model)) if !namespace.is_empty() => model,
        _ => served_model,
    }
}

/// The 供应商 display of one attempt: the account it landed on and the model
/// the upstream actually served, joined `account/model` —
/// `cc/deepseek-v4.1-flash` — one shape for the 动态 feed and the 请求 list.
/// The model's vendor namespace is stripped ([`strip_model_namespace`]). A
/// side that is absent or empty leaves the other alone; both absent read 「—」.
pub(crate) fn account_model(account: Option<&str>, served_model: Option<&str>) -> String {
    let account = account.filter(|account| !account.is_empty());
    let model = served_model
        .map(strip_model_namespace)
        .filter(|model| !model.is_empty());
    match (account, model) {
        (Some(account), Some(model)) => format!("{account}/{model}"),
        (Some(account), None) => account.to_string(),
        (None, Some(model)) => model.to_string(),
        (None, None) => "—".to_string(),
    }
}

/// Micro-USD per USD.
const MICROUS_PER_USD: u64 = 1_000_000;

/// USD for lists and balances, from micro-USD, integer arithmetic only.
///
/// Rules (one style for every page that uses it):
/// - at most 4 decimals: the 5th and 6th micro-USD digits are rounded half
///   away from zero (`12_350` → `$0.0124`, `12_349` → `$0.0123`);
/// - trailing fractional zeros are trimmed, but at least 2 decimals stay
///   (`10_000_000` → `$10.00`, `10_499_700` → `$10.4997`);
/// - a negative amount puts the minus before the dollar sign
///   (`-500_000` → `-$0.50`); one that rounds to zero shows no sign.
///
/// Use [`usd_exact`] where a 1 µUSD difference must stay visible.
pub(crate) fn usd_microus(microus: i64) -> String {
    let magnitude = microus.unsigned_abs();
    // 1e-4 USD units; `u64::MAX` leaves room for the +50 on `i64::MIN`.
    let ten_thousandths = (magnitude + 50) / 100;
    let digits = format!("{:04}", ten_thousandths % 10_000);
    dollars(
        microus < 0 && ten_thousandths != 0,
        ten_thousandths / 10_000,
        &digits,
    )
}

/// Exact USD from micro-USD: every significant digit of the six kept, with
/// the same sign and trailing-zero rule as [`usd_microus`]
/// (`-1_234_567` → `-$1.234567`, `2_000_000` → `$2.00`). For confirm
/// previews and reconciliation, where rounding could hide a difference.
pub(crate) fn usd_exact(microus: i64) -> String {
    let magnitude = microus.unsigned_abs();
    let digits = format!("{:06}", magnitude % MICROUS_PER_USD);
    dollars(microus < 0, magnitude / MICROUS_PER_USD, &digits)
}

/// `[-]$whole.fraction` with `fraction_digits` trimmed of trailing zeros down
/// to two digits.
fn dollars(negative: bool, whole: u64, fraction_digits: &str) -> String {
    let trimmed = fraction_digits.trim_end_matches('0');
    let fraction = if trimmed.len() < 2 {
        &fraction_digits[..2]
    } else {
        trimmed
    };
    let sign = if negative { "-" } else { "" };
    format!("{sign}${whole}.{fraction}")
}

/// How long ago a quota snapshot was probed, humanized for the card header:
/// <60s 「刚刚」, <1h 「N 分钟前」, <24h 「N 小时前」, else 「N 天前」.
/// A future timestamp (clock skew) reads as 「刚刚」.
pub(crate) fn probe_age(
    probed_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = now.signed_duration_since(probed_at).num_seconds().max(0);
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}
