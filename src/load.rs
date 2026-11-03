//! The GUI's own config-file access: which file is open (for the settings
//! page's write-back) and the `[gui]` section of it. The gateway-facing
//! `[gateway]` section is parsed by `gateway::config::Config::load` — this
//! module is only what the GUI itself needs on top of that.

use std::path::{Path, PathBuf};

/// The GUI-specific section of the main config file. Unknown fields are
/// ignored (serde's default for a struct without `deny_unknown_fields`), so
/// the GUI never chokes on keys a newer version writes.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct GuiSection {
    /// Display timezone, `±HH:MM`; omitted/empty = follow the system.
    pub timezone: Option<String>,
    /// 销售模式: whether this deployment sells usage (概览 shows 营收). Omitted =
    /// not selling.
    pub sales_mode: Option<bool>,
}

/// Load the `[gui]` section of `path`. A missing file or section yields the
/// defaults; a malformed section degrades to defaults with a warning — the
/// GUI must start and the settings page surfaces what it parsed.
pub fn gui_section(path: &Path) -> GuiSection {
    /// Extraction view: everything outside `[gui]` belongs to other sections
    /// (`[gateway]`, `[log]`) and is ignored here.
    #[derive(serde::Deserialize)]
    struct MainConfig {
        gui: Option<GuiSection>,
    }

    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return GuiSection::default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read config; [gui] defaults");
            return GuiSection::default();
        }
    };
    match toml::from_str::<MainConfig>(&raw) {
        Ok(main) => main.gui.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to parse config; [gui] defaults");
            GuiSection::default()
        }
    }
}

/// The open config file's path, process-global: set once at startup from
/// `--config` (or the default `~/.ox/config.toml`), read by the settings page
/// to write `[gui].timezone` back to the file the user actually launched with.
static CONFIG_PATH: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Record the open config file's path (called once from `run`).
pub(crate) fn set_config_path(path: PathBuf) {
    if let Ok(mut slot) = CONFIG_PATH.write() {
        *slot = Some(path);
    }
}

/// The open config file's path for write-back; `None` only when startup
/// never resolved one (no home directory and no `--config` — practically
/// unreachable).
pub(crate) fn config_path() -> Option<PathBuf> {
    CONFIG_PATH.read().ok().and_then(|slot| slot.clone())
}

/// Edit the `[gui]` section of `path` in place — the settings page's
/// write-back, shared by every GUI setting so each one only has to describe its
/// own key. The rest of the document is preserved; a missing file or section is
/// created, and an `[gui]` table left empty is dropped rather than written as a
/// bare header. Errors are strings for the caller to show in place.
pub(crate) fn write_gui_section(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::Table),
) -> Result<(), String> {
    use std::fmt::Write as _;

    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("读取配置 {} 失败: {error}", path.display()))?;
    let mut document = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("解析配置 {} 失败: {error}", path.display()))?;
    let gui = document
        .entry("gui")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let table = gui
        .as_table_mut()
        .ok_or_else(|| "配置的 [gui] 不是表".to_string())?;
    edit(table);
    // Keep the section tidy: a table emptied by removing its last key writes as
    // `[gui]` with no body — harmless, but drop it entirely.
    if table.is_empty() {
        document.remove("gui");
    }
    let mut output = String::new();
    write!(output, "{document}").map_err(|error| format!("序列化配置失败: {error}"))?;
    std::fs::write(path, output)
        .map_err(|error| format!("写入配置 {} 失败: {error}", path.display()))
}
