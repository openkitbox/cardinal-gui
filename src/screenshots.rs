//! Offscreen screenshot rendering for the GUI's mock-data visual check.
//!
//! This module is the crate-internal assembly point: it can reach private
//! items (`apply_light_theme`, `GuiAssets`, `App`, page types, `GatewayRuntime`)
//! that the `screenshots` binary — a separate crate — cannot. The binary only
//! calls [`crate::run_screenshots`].

mod fixtures;
mod mock_server;

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{AnyWindowHandle, AppContext as _, Entity, HeadlessAppContext, Window, px, size};
use gpui_kit::{component::Root, test::TestWindowExt as _};

use crate::{App, app::StatusTab, runtime};

use self::mock_server::MockServer;

const VIEWPORT_WIDTH: u32 = 1160;
const VIEWPORT_HEIGHT: u32 = 1020;
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// An individual admin call can be dropped before it reaches the mock server
/// (a dropped GPUI task or a transient link state). Re-driving the action a
/// few times is what makes the tool reliable; each attempt still gates on a
/// concrete end state with its own hard deadline.
const ACTION_ATTEMPTS: u8 = 3;

/// Which page-extra action a scene needs before capture.
#[derive(Clone, Copy)]
enum SceneAction {
    None,
    NewKeyForm,
    EditKeyForm,
    SettingsPage(usize),
}

struct Scene {
    name: &'static str,
    tab: StatusTab,
    action: SceneAction,
}

/// The supported scene table. Each entry produces one PNG in
/// `.ox/screenshots/out/<name>.png`.
const SCENES: &[Scene] = &[
    Scene {
        name: "providers-cards",
        tab: StatusTab::Providers,
        action: SceneAction::None,
    },
    Scene {
        name: "providers-form-new",
        tab: StatusTab::Providers,
        action: SceneAction::NewKeyForm,
    },
    Scene {
        name: "providers-form-edit",
        tab: StatusTab::Providers,
        action: SceneAction::EditKeyForm,
    },
    Scene {
        name: "routes",
        tab: StatusTab::Routes,
        action: SceneAction::None,
    },
    Scene {
        name: "usage",
        tab: StatusTab::Usage,
        action: SceneAction::None,
    },
    Scene {
        name: "users",
        tab: StatusTab::Users,
        action: SceneAction::None,
    },
    Scene {
        name: "settings-general",
        tab: StatusTab::Settings,
        action: SceneAction::SettingsPage(0),
    },
    Scene {
        name: "settings-mail",
        tab: StatusTab::Settings,
        action: SceneAction::SettingsPage(1),
    },
    Scene {
        name: "settings-providers",
        tab: StatusTab::Settings,
        action: SceneAction::SettingsPage(2),
    },
    Scene {
        name: "settings-payment",
        tab: StatusTab::Settings,
        action: SceneAction::SettingsPage(3),
    },
];

pub(crate) fn run() -> Result<(), String> {
    let fixture = fixtures::load()?;
    let edit_target = fixture.edit_target.clone();
    let server = MockServer::start(fixture.routes)?;

    let config = gateway::config::Config {
        listen: server.base().to_owned(),
        ..Default::default()
    };

    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        std::sync::Arc::new(crate::assets::GuiAssets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(gpui_kit::init);
    cx.update(crate::apply_light_theme);
    // Fix the animation state so the TabBar indicator renders at its final
    // position instead of an in-flight frame (200ms slide in gpui-kit 0.10).
    cx.update(|app_cx| app_cx.set_reduce_motion(true));
    cx.update(|app_cx| {
        app_cx.set_global(runtime::spawn(config));
    });

    let app_slot: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
    let slot = app_slot.clone();
    let window_handle = cx
        .open_window(size(px(1160.), px(1020.)), |window: &mut Window, cx| {
            let view = cx.new(|cx| App::new(window, cx));
            *slot.borrow_mut() = Some(view.clone());
            cx.new(|cx| Root::new(view, window, cx))
        })
        .map_err(|e| format!("open headless window: {e:#}"))?;
    let any_window: AnyWindowHandle = window_handle.into();
    let app = app_slot
        .borrow()
        .clone()
        .ok_or("App entity was not created in the headless window")?;

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".ox/screenshots/out");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    // Admin calls are dropped until the startup probe flips the gateway link
    // to Connected. Waiting for that state first is what makes every later
    // "data loaded" condition achievable instead of timing out with no HTTP.
    let mut connected = false;
    for attempt in 0..ACTION_ATTEMPTS {
        if wait_for(&mut cx, "gateway link to become Connected", |cx| {
            cx.update_window(any_window, |_, _, app_cx| {
                app.read(app_cx).gateway_connected_for_screenshot()
            })
            .unwrap_or(false)
        })
        .is_ok()
        {
            connected = true;
            break;
        }
        // A probe can be lost before it sends /health; re-run it on the admin
        // runtime rather than waiting forever on a link that never resolves.
        cx.update(|app_cx| app_cx.global::<runtime::GatewayRuntime>().reconnect());
        if attempt + 1 == ACTION_ATTEMPTS {
            return Err(format!(
                "gateway link never became Connected after {ACTION_ATTEMPTS} probes; mock paths = {:?}",
                server.seen_paths()
            ));
        }
    }
    if !connected {
        return Err("gateway link not Connected".to_owned());
    }

    for scene in SCENES {
        // Phase 1 — activate the tab and wait for that page's data. Each
        // attempt re-issues the reload, because an admin read can be dropped
        // before it reaches the server; the explicit end state is the gate.
        let mut loaded = false;
        for attempt in 0..ACTION_ATTEMPTS {
            cx.update_window(any_window, |_, window, app_cx| {
                let (state, status_page) = app.read(app_cx).screenshot_handles();
                state.update(app_cx, |state, cx| state.set_status_tab(scene.tab, cx));
                status_page.update(app_cx, |page, cx| {
                    page.activate_tab_for_screenshot(scene.tab, cx)
                });
                window.render_frame(app_cx);
            })
            .map_err(|e| format!("select tab for {}: {e:#}", scene.name))?;

            if wait_for(&mut cx, &format!("{} data to load", scene.name), |cx| {
                page_loaded(cx, &app, any_window, scene.tab)
            })
            .is_ok()
            {
                loaded = true;
                break;
            }
            if attempt + 1 == ACTION_ATTEMPTS {
                let debug = cx.update_window(any_window, |_, _, app_cx| {
                    let (_, status_page) = app.read(app_cx).screenshot_handles();
                    status_page.read(app_cx).screenshot_debug(app_cx)
                });
                return Err(format!(
                    "scene {}: page data never loaded after {ACTION_ATTEMPTS} attempts; {debug:?}",
                    scene.name
                ));
            }
        }
        if !loaded {
            return Err(format!("scene {}: page data not ready", scene.name));
        }

        // Phase 2 — only now apply the form action, and wait for its own
        // explicit completion signal rather than a global request count.
        let mut requests_before = server.seen_paths().len();
        if !matches!(scene.action, SceneAction::None) {
            let mut opened = false;
            for attempt in 0..ACTION_ATTEMPTS {
                requests_before = server.seen_paths().len();
                cx.update_window(any_window, |_, window, app_cx| {
                    let (_, status_page) = app.read(app_cx).screenshot_handles();
                    match scene.action {
                        SceneAction::NewKeyForm => {
                            let providers = status_page.read(app_cx).providers_entity();
                            providers.update(app_cx, |page, cx| {
                                page.open_key_form_for_screenshot(window, cx)
                            });
                        }
                        SceneAction::EditKeyForm => {
                            let providers = status_page.read(app_cx).providers_entity();
                            providers.update(app_cx, |page, cx| {
                                page.open_key_editor_for_screenshot(
                                    edit_target.provider_id,
                                    edit_target.key_id,
                                    window,
                                    cx,
                                )
                            });
                        }
                        SceneAction::SettingsPage(page_ix) => {
                            let settings = status_page.read(app_cx).settings_entity();
                            settings.update(app_cx, |page, cx| {
                                page.set_screenshot_page(page_ix, cx);
                            });
                        }
                        SceneAction::None => {}
                    }
                    window.render_frame(app_cx);
                })
                .map_err(|e| format!("prepare scene {}: {e:#}", scene.name))?;

                if wait_for(&mut cx, &format!("{} form action", scene.name), |cx| {
                    form_open(cx, &app, any_window, scene.action)
                })
                .is_ok()
                {
                    opened = true;
                    break;
                }
                if attempt + 1 == ACTION_ATTEMPTS {
                    let debug = cx.update_window(any_window, |_, _, app_cx| {
                        let (_, status_page) = app.read(app_cx).screenshot_handles();
                        status_page.read(app_cx).screenshot_debug(app_cx)
                    });
                    return Err(format!(
                        "scene {}: form action never completed after {ACTION_ATTEMPTS} attempts; {debug:?}",
                        scene.name
                    ));
                }
            }
            if !opened {
                return Err(format!("scene {}: form not open", scene.name));
            }
        }

        // Fail closed: a fixture id drift that stops the editor from fetching
        // its credential must not silently produce an empty form. The wait
        // above already gates on the response landing; this keeps the check
        // explicit at the capture boundary.
        if matches!(scene.action, SceneAction::EditKeyForm) {
            let seen = server.seen_paths();
            let fresh = &seen[requests_before..];
            if !fresh.contains(&edit_target.reveal_path) {
                return Err(format!(
                    "scene {}: expected credential reveal {} was never requested (saw {fresh:?}); check the edit key in the fixture",
                    scene.name, edit_target.reveal_path
                ));
            }
        }

        cx.update_window(any_window, |_, window, app_cx| window.render_frame(app_cx))
            .map_err(|e| format!("render scene {}: {e:#}", scene.name))?;

        let shot = cx.capture_screenshot(any_window).map_err(|e| {
            format!(
                "capture {} — no headless renderer (needs Metal/WGPU): {e:#}",
                scene.name
            )
        })?;
        let resized = fit_viewport(&shot);
        let path = out_dir.join(format!("{}.png", scene.name));
        resized
            .save(&path)
            .map_err(|e| format!("save {}: {e}", path.display()))?;
        println!("wrote {}", path.display());
    }

    println!(
        "mock server served {} requests across {:?}",
        server.hits(),
        server.seen_paths()
    );
    Ok(())
}

/// Poll a concrete readiness condition with a hard deadline. `run_until_parked`
/// pumps GPUI's dispatcher, which is separate from the gateway's tokio runtime;
/// the short yield lets in-flight loopback requests make progress.
fn wait_for(
    cx: &mut HeadlessAppContext,
    label: &str,
    mut ready: impl FnMut(&mut HeadlessAppContext) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        cx.run_until_parked();
        if ready(cx) {
            // Let the satisfied state reach the render tree before returning.
            cx.run_until_parked();
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out after 10s waiting for {label}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn page_loaded(
    cx: &mut HeadlessAppContext,
    app: &Entity<App>,
    window: AnyWindowHandle,
    tab: StatusTab,
) -> bool {
    cx.update_window(window, |_, _, app_cx| {
        let (_, status_page) = app.read(app_cx).screenshot_handles();
        status_page
            .read(app_cx)
            .page_loaded_for_screenshot(tab, app_cx)
    })
    .unwrap_or(false)
}

fn form_open(
    cx: &mut HeadlessAppContext,
    app: &Entity<App>,
    window: AnyWindowHandle,
    action: SceneAction,
) -> bool {
    cx.update_window(window, |_, _, app_cx| {
        let (_, status_page) = app.read(app_cx).screenshot_handles();
        let providers = status_page.read(app_cx).providers_entity();
        let page = providers.read(app_cx);
        match action {
            SceneAction::None | SceneAction::SettingsPage(_) => true,
            SceneAction::NewKeyForm => page.has_key_form_for_screenshot(),
            // The reveal response must have landed, not just been requested:
            // the form shows the placeholder until `api_key` is filled in.
            SceneAction::EditKeyForm => page.editor_key_revealed_for_screenshot(),
        }
    })
    .unwrap_or(false)
}

/// The headless platform renders at 2× device pixels; downsample to the exact
/// logical viewport so every baseline is comparable across machines.
fn fit_viewport(image: &image::RgbaImage) -> image::RgbaImage {
    let (width, height) = image.dimensions();
    if width == VIEWPORT_WIDTH && height == VIEWPORT_HEIGHT {
        image.clone()
    } else {
        image::imageops::resize(
            image,
            VIEWPORT_WIDTH,
            VIEWPORT_HEIGHT,
            image::imageops::FilterType::Lanczos3,
        )
    }
}
