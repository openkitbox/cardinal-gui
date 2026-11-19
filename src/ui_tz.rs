//! The GUI display timezone, backed by Jiff. Configured values are fixed
//! offsets; an omitted value uses the current system offset, not historical
//! timezone rules for the date being displayed.

use std::sync::RwLock;

use jiff::Timestamp;
use jiff::tz::{Offset, TimeZone};

use crate::load::GuiSection;

static DISPLAY_TZ: RwLock<Option<Offset>> = RwLock::new(None);

pub(crate) fn display_tz() -> TimeZone {
    let offset =
        configured_offset().unwrap_or_else(|| TimeZone::system().to_offset(Timestamp::now()));
    TimeZone::fixed(offset)
}

pub(crate) fn init(section: &GuiSection) {
    let raw = section.timezone.as_deref().map(str::trim);
    let parsed = raw.and_then(parse_offset);
    if let Some(raw) = raw.filter(|raw| !raw.is_empty() && parsed.is_none()) {
        tracing::warn!(
            timezone = raw,
            "invalid [gui].timezone; following the system clock"
        );
    }
    set_display_tz(parsed);
}

pub(crate) fn set_display_tz(offset: Option<Offset>) {
    if let Ok(mut slot) = DISPLAY_TZ.write() {
        *slot = offset;
    }
}

pub(crate) fn configured_timezone() -> Option<String> {
    configured_offset().map(|offset| format_offset(offset.seconds()))
}

fn configured_offset() -> Option<Offset> {
    DISPLAY_TZ.read().ok().and_then(|slot| *slot)
}

/// Parse a trimmed `±HH:MM`, `±HHMM`, or `±HH` offset.
fn parse_offset(raw: &str) -> Option<Offset> {
    let (sign, digits) = raw.as_bytes().split_first()?;
    let sign = match sign {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = match *digits {
        [h1, h2, b':', m1, m2] | [h1, h2, m1, m2] => (decimal_pair(h1, h2)?, decimal_pair(m1, m2)?),
        [h1, h2] => (decimal_pair(h1, h2)?, 0),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}

fn decimal_pair(tens: u8, ones: u8) -> Option<i32> {
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    Some(10 * i32::from(tens - b'0') + i32::from(ones - b'0'))
}

fn format_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.unsigned_abs();
    format!("{sign}{:02}:{:02}", seconds / 3600, seconds / 60 % 60)
}

pub(crate) fn parse_timezone_input(raw: &str) -> Result<Option<Offset>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_offset(trimmed)
        .map(Some)
        .ok_or_else(|| format!("无法解析时区「{trimmed}」：请用 ±HH:MM 形式，如 +08:00"))
}

pub(crate) fn save_timezone(
    config_path: &std::path::Path,
    value: Option<&str>,
) -> Result<(), String> {
    crate::load::write_gui_section(config_path, |table| match value {
        Some(value) => table["timezone"] = toml_edit::value(value),
        None => {
            table.remove("timezone");
        }
    })
}
