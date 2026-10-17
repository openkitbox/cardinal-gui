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
