//! Composite asset source: a handful of vendored Lucide icons on top of
//! gpui-kit's 106-icon default bundle.
//!
//! Why this exists: `gpui_kit::assets::Assets` embeds only the component
//! library's default icon list, so an `IconName` outside that list (e.g.
//! `Activity`, `MessageSquare`) compiles fine but renders nothing. The full
//! catalog (`AllAssets`) would embed 1830 SVGs (~7 MB) to fix three icons.
//! Icons are vendored from lucide.dev (ISC license) into assets/icons/ — add
//! one line here per new out-of-bundle icon.

use gpui::{AssetSource, Result, SharedString};
use gpui_kit::assets::Assets;
use std::borrow::Cow;

const VENDORED: [(&str, &[u8]); 8] = [
    (
        "icons/activity.svg",
        include_bytes!("../assets/icons/activity.svg"),
    ),
    (
        "icons/bitcoin.svg",
        include_bytes!("../assets/icons/bitcoin.svg"),
    ),
    (
        "icons/grip-vertical.svg",
        include_bytes!("../assets/icons/grip-vertical.svg"),
    ),
    (
        "icons/message-square.svg",
        include_bytes!("../assets/icons/message-square.svg"),
    ),
    (
        "icons/messages-square.svg",
        include_bytes!("../assets/icons/messages-square.svg"),
    ),
    (
        "icons/pencil.svg",
        include_bytes!("../assets/icons/pencil.svg"),
    ),
    (
        "icons/sparkles.svg",
        include_bytes!("../assets/icons/sparkles.svg"),
    ),
    ("icons/zap.svg", include_bytes!("../assets/icons/zap.svg")),
];

/// The macOS Dock icon (icns bundle).
#[cfg(target_os = "macos")]
pub(crate) const APP_ICON: &[u8] = include_bytes!("../assets/AppIcon.icns");

/// Provider brand logos embedded in the binary, one constant per catalog
/// vendor (GLM is the zhipu file). Referenced by pages/providers/catalog.rs,
/// so every asset path string lives in this one module. Rendered via `img()`
/// (full color), not the AssetSource path — gpui's `svg()` element renders
/// monochrome alpha-masks.
pub(crate) mod provider_logos {
    pub(crate) const ANTHROPIC: &[u8] = include_bytes!("../assets/providers/anthropic.svg");
    pub(crate) const DEEPSEEK: &[u8] = include_bytes!("../assets/providers/deepseek.svg");
    pub(crate) const GLM: &[u8] = include_bytes!("../assets/providers/zhipu.svg");
    pub(crate) const KIMI: &[u8] = include_bytes!("../assets/providers/kimi.svg");
    /// Gemini (lobe-icons full-color mark).
    pub(crate) const GEMINI: &[u8] = include_bytes!("../assets/providers/gemini.svg");
    pub(crate) const OPENAI: &[u8] = include_bytes!("../assets/providers/openai.svg");
    /// OpenCode (the two-rect mark).
    pub(crate) const OPENCODE: &[u8] = include_bytes!("../assets/providers/opencode.svg");
    /// Command Code (the knot mark).
    pub(crate) const COMMANDCODE: &[u8] = include_bytes!("../assets/providers/commandcode.svg");
}

pub(crate) struct GuiAssets;

impl AssetSource for GuiAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, data)) = VENDORED.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(*data)));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed = Assets.list(path)?;
        listed.extend(
            VENDORED
                .iter()
                .filter(|(name, _)| name.starts_with(path))
                .map(|(name, _)| SharedString::from(*name)),
        );
        Ok(listed)
    }
}
