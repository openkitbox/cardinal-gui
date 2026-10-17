//! cardinal-gui: a native GPUI client for monitoring and managing the
//! cardinal gateway.
//!
//! The gateway relay is a **separate process** (`cardinal-gateway`); the GUI
//! is a pure HTTP client of its loopback admin API — plus an SSE subscription
//! to the live activity feed — via the tokio runtime bridge in [`runtime`].
//!
//! UI foundation: gpui-kit 0.7 (`gpui_kit::*` re-exports gpui-pre; styled
//! components come from `gpui_kit::component`, initialized via [`init`] and
//! wrapped in a [`Root`]).

mod activity;
mod app;
mod assets;
mod fmt;
pub mod load;
mod pages;
mod runtime;
mod ui_tz;

use app::App;

use crate::assets::GuiAssets;
use gpui::{AppContext as _, WindowBounds, px, size};
use gpui_kit::{application, component::Root, init, open_window};

/// Install the light palette (GitHub-light family) onto the component
/// library's light theme. This is the single color definition point — every
/// view reads `cx.theme()`. Light-only: no theme switching.
fn apply_light_theme(cx: &mut gpui::App) {
    use gpui_kit::component::{Theme, ThemeMode};

    const BG: u32 = 0xFFFFFF;
    const SIDEBAR: u32 = 0xF6F8FA;
    const CARD: u32 = 0xF6F8FA;
    const SELECTED: u32 = 0xE4E9EE;
    const MUTED: u32 = 0xEAEEF2;
    const BORDER: u32 = 0xD8DEE4;
    const TEXT: u32 = 0x1F2328;
    const TEXT_DIMMED: u32 = 0x656D76;
    const PRIMARY: u32 = 0x0969DA;
    const GREEN: u32 = 0x1A7F37;
    const DESTRUCTIVE: u32 = 0xCF222E;
    const SIDEBAR_FG: u32 = 0x24292F;

    Theme::change(ThemeMode::Light, None, cx);
    Theme::update(cx, |theme| {
        let c = &mut theme.colors;
        c.background = gpui::rgb(BG).into();
        c.foreground = gpui::rgb(TEXT).into();
        c.border = gpui::rgb(BORDER).into();
        c.muted = gpui::rgb(MUTED).into();
        c.muted_foreground = gpui::rgb(TEXT_DIMMED).into();
        c.primary = gpui::rgb(PRIMARY).into();
        c.primary_foreground = gpui::rgb(BG).into();
        c.accent = gpui::rgb(SELECTED).into();
        c.accent_foreground = gpui::rgb(TEXT).into();
        c.secondary = gpui::rgb(MUTED).into();
        c.secondary_foreground = gpui::rgb(TEXT_DIMMED).into();
        c.popover = gpui::rgb(CARD).into();
        c.popover_foreground = gpui::rgb(TEXT).into();
        c.sidebar = gpui::rgb(SIDEBAR).into();
        c.sidebar_foreground = gpui::rgb(SIDEBAR_FG).into();
        c.sidebar_border = gpui::rgb(BORDER).into();
        c.sidebar_accent = gpui::rgb(SELECTED).into();
        c.sidebar_accent_foreground = gpui::rgb(SIDEBAR_FG).into();
        c.sidebar_primary = gpui::rgb(PRIMARY).into();
        c.sidebar_primary_foreground = gpui::rgb(BG).into();
        c.input = gpui::rgb(BORDER).into();
        c.ring = gpui::rgb(PRIMARY).into();
        c.caret = gpui::rgb(PRIMARY).into();
        c.selection = gpui::rgb(SELECTED).into();
        c.success = gpui::rgb(GREEN).into();
        c.danger = gpui::rgb(DESTRUCTIVE).into();
        c.scrollbar_thumb = gpui::rgb(MUTED).into();
        c.list_active = gpui::rgb(SELECTED).into();
        c.list_active_border = gpui::rgb(PRIMARY).into();

        // Match the app's 13px base / 12px mono type scale.
        theme.font_size = px(13.);
        theme.mono_font_size = px(12.);
    });
}

/// Set the Dock / app-switcher icon from the bundled icns. macOS-only: the
/// binary runs without an .app bundle, so the icon must be injected at
/// runtime through AppKit (gpui exposes no icon API).
///
/// Must run **inside** gpui's `run` closure: gpui-pre-macos registers a
/// `GPUIApplication` subclass carrying its `platform` ivar, and the shared
/// application instance must be created through that subclass. Touching
/// `NSApplication::sharedApplication` before gpui boots would create a
/// base-class instance and crash gpui with "Ivar platform not found".
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::from_vec(crate::assets::APP_ICON.to_vec());
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

#[cfg(not(target_os = "macos"))]
fn set_dock_icon() {}

/// Per-feature configs handed to the GUI by the caller: each leaf feature
/// owns a section of the main config file (`~/.ox/config.toml`).
pub struct Configs {
    /// `[gateway]` — the GUI reads the relay's listen address and admin key
    /// from it (it no longer hosts the relay).
    pub gateway: gateway::config::Config,
    /// `[gui]` — GUI-specific settings (display timezone), from the same file.
    pub gui: load::GuiSection,
    /// The open config file's path (for the settings page's write-back); `None`
    /// only when startup never resolved one.
    pub config_path: Option<std::path::PathBuf>,
}

/// Launch the GUI: owns the main thread (macOS AppKit requirement) and never
/// returns until the window quits. `configs` bundles the per-feature sections
/// of the main config file.
pub fn run(configs: Configs) {
    // Install the display timezone and the open config path before any page
    // renders — every timestamp formatter reads `ui_tz::display_tz()`, and the
    // settings page writes back to `load::config_path()`.
    ui_tz::init(&configs.gui);
    if let Some(path) = configs.config_path {
        load::set_config_path(path);
    }
    application()
        .with_assets(GuiAssets)
        .run(|cx: &mut gpui::App| {
            // Initialize the component library (themes, icons, assets) before any
            // component is used, then restyle it as GitHub-light.
            init(cx);
            apply_light_theme(cx);
            set_dock_icon();

            // Probe the standalone gateway on its own tokio thread before any page
            // can ask for data; an unreachable gateway is a rendered state, never a
            // blocked window.
            let gateway_runtime = runtime::spawn(configs.gateway);
            cx.set_global(gateway_runtime);

            let bounds = gpui::Bounds::centered(None, size(px(1160.), px(1020.)), cx);
            // Fixed-size window: the layout (300px cards, sidebar) is designed for this
            // exact size, so edge-drag resizing and the zoom button are disabled.
            let options = gpui::WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                is_resizable: false,
                ..Default::default()
            };
            open_window(options, cx, |window, cx| {
                window.set_window_title("Cardinal GUI");
                let view = cx.new(|cx| App::new(window, cx));
                // `Root` is the component library's required window wrapper.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("open main window");
            cx.activate(true);
        });
}
