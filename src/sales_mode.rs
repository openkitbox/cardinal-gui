//! 销售模式: whether this deployment sells usage, or only reports what it cost.
//!
//! ```toml
//! [gui]
//! sales_mode = true   # omitted/false = not selling
//! ```
//!
//! Off — the default — the 概览 page reports no 营收 at all (a relay nobody is
//! billed through has no revenue). The switch is display-only: the gateway keeps
//! returning the charged amount, and no label changes with it — cost figures are
//! 金额 everywhere, because there is no price list to advertise.
//!
//! Process-global like [`crate::ui_tz`]'s offset: `init` installs the startup
//! value once, the settings page writes a toggle through, and every reader picks
//! it up on its next render.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::load::GuiSection;

/// Whether this deployment sells usage. Off until the config says otherwise, so
/// a missing `[gui]`, a missing key and a malformed section all read as "not
/// selling".
static SALES_MODE: AtomicBool = AtomicBool::new(false);

/// Install the startup value from the loaded `[gui]` section (called once from
/// `run`).
pub(crate) fn init(section: &GuiSection) {
    SALES_MODE.store(section.sales_mode.unwrap_or(false), Ordering::Relaxed);
}

/// Whether the deployment sells usage: 概览 shows 营收. Render boundaries read
/// this once and pass the bool down, so formatting stays pure.
pub(crate) fn enabled() -> bool {
    SALES_MODE.load(Ordering::Relaxed)
}

/// Apply a toggle immediately, after its config write has landed.
pub(crate) fn set_enabled(on: bool) {
    SALES_MODE.store(on, Ordering::Relaxed);
}

/// Persist the switch to the open config file. `false` removes the key instead of
/// writing it — off is the default, so the file only spells out deviations.
pub(crate) fn save(config_path: &std::path::Path, on: bool) -> Result<(), String> {
    crate::load::write_gui_section(config_path, |table| {
        if on {
            table["sales_mode"] = toml_edit::value(true);
        } else {
            table.remove("sales_mode");
        }
    })
}
