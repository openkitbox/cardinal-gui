//! The GUI's display timezone — one process-global offset every page formats
//! with, from the `[gui]` section of the main config file:
//!
//! ```toml
//! [gui]
//! timezone = "+08:00"   # fixed UTC offset; omitted/empty = follow the system
//! ```
//!
//! Fixed offsets only (`chrono::FixedOffset`): a dashboard showing usage data
//! needs a stable, unambiguous wall clock, and IANA zones would pull in
//! chrono-tz for zero display benefit. The settings page edits this value and
//! writes it back to the open config file (`save_timezone`), so a choice
//! persists across restarts; it takes effect immediately because every reader
//! goes through [`display_tz`].

use std::sync::RwLock;

use chrono::FixedOffset;

use crate::load::GuiSection;

/// The process-global display timezone: `Some` = the configured fixed offset,
/// `None` = follow the system (snapshot of the local offset at read time).
static DISPLAY_TZ: RwLock<Option<FixedOffset>> = RwLock::new(None);

/// The offset every page formats timestamps with. `None` (follow the system)
/// snapshots the current local offset — stable across a DST transition within
/// one render pass only in the same sense as `chrono::Local` itself.
pub(crate) fn display_tz() -> FixedOffset {
    let configured = DISPLAY_TZ.read().ok().and_then(|slot| *slot);
    match configured {
        Some(offset) => offset,
        None => *chrono::Local::now().offset(),
    }
}

/// Install the startup value from the loaded `[gui]` section (called once
/// from `run`). An invalid string degrades to follow-the-system with a
/// warning — the GUI must start, the settings page shows what it parsed.
pub(crate) fn init(section: &GuiSection) {
    let parsed = section.timezone.as_deref().and_then(parse_offset);
    if let Some(raw) = section
        .timezone
        .as_deref()
        .filter(|raw| parse_offset(raw).is_none())
    {
        tracing::warn!(
            timezone = raw,
            "invalid [gui].timezone; following the system clock"
        );
    }
    if let Ok(mut slot) = DISPLAY_TZ.write() {
        *slot = parsed;
    }
}

/// Change the display timezone immediately (the settings page's save): all
/// pages pick the new offset up on their next render.
pub(crate) fn set_display_tz(offset: Option<FixedOffset>) {
    if let Ok(mut slot) = DISPLAY_TZ.write() {
        *slot = offset;
    }
}

/// The currently configured value as the settings page shows it: `None` (or
/// an empty string) means follow the system.
pub(crate) fn configured_timezone() -> Option<String> {
    DISPLAY_TZ
        .read()
        .ok()
        .and_then(|slot| *slot)
        .map(|offset| offset.to_string())
}

/// Parse a `±HH:MM` (or `±HHMM`/`±HH`) offset string. Empty/whitespace →
/// `None` (follow the system). chrono's `FixedOffset::from_str` already
/// accepts exactly these forms.
fn parse_offset(raw: &str) -> Option<FixedOffset> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<FixedOffset>().ok()
}

/// Parse the settings-page input: empty = follow the system (`None`),
/// otherwise a fixed offset. Distinguishes "clear the setting" from "invalid
/// input" so the caller can reject the latter.
pub(crate) fn parse_timezone_input(raw: &str) -> Result<Option<FixedOffset>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<FixedOffset>()
        .map(Some)
        .map_err(|_| format!("无法解析时区「{trimmed}」：请用 ±HH:MM 形式，如 +08:00"))
}

/// Write the `[gui].timezone` value back to the open config file, preserving
/// the rest of the document byte-for-byte where possible. A missing file or
/// `[gui]` section is created; an empty value removes the key. Returns an
/// error string on any IO/parse failure — the caller shows it in place.
pub(crate) fn save_timezone(
    config_path: &std::path::Path,
    value: Option<&str>,
) -> Result<(), String> {
    use std::fmt::Write as _;

    let raw = std::fs::read_to_string(config_path)
        .map_err(|error| format!("读取配置 {} 失败: {error}", config_path.display()))?;
    let mut document = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("解析配置 {} 失败: {error}", config_path.display()))?;
    let gui = document
        .entry("gui")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let table = gui
        .as_table_mut()
        .ok_or_else(|| "配置的 [gui] 不是表".to_string())?;
    match value {
        Some(value) => {
            table["timezone"] = toml_edit::value(value);
        }
        None => {
            table.remove("timezone");
        }
    }
    // Keep the section tidy: an empty [gui] table after removing the key
    // writes as `[gui]` with no body — harmless, but drop it entirely when it
    // was only carrying this key.
    if table.is_empty() {
        document.remove("gui");
    }
    let mut output = String::new();
    write!(output, "{document}").map_err(|error| format!("序列化配置失败: {error}"))?;
    std::fs::write(config_path, output)
        .map_err(|error| format!("写入配置 {} 失败: {error}", config_path.display()))
}
