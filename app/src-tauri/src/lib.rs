mod syncer;

use tauri::{
    image::Image,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri_plugin_positioner::{Position, WindowExt};

/// The positioner panics on tray-relative positions until it has observed a
/// tray event, so track whether one has happened in this process.
static TRAY_SEEN: AtomicBool = AtomicBool::new(false);

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// First real engine call from the UI: which tools exist on this machine.
#[tauri::command]
fn detect_tools() -> Vec<codesync_engine::adapters::DetectedTool> {
    codesync_engine::adapters::detect_all()
}

#[tauri::command]
fn engine_version() -> &'static str {
    codesync_engine::VERSION
}

#[tauri::command]
async fn get_status(app: tauri::AppHandle) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Ensure a store exists (first run): writes the default local-folder config
/// if none is present. Real backend selection wires into onboarding later.
#[tauri::command]
async fn configure_default_store(app: tauri::AppHandle) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        if syncer::load_config(&paths)?.is_none() {
            syncer::save_config(&paths, &syncer::default_config()?)?;
        }
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Toggle the opt-in plugins scope (never on by default — can be large).
#[tauri::command]
async fn set_sync_plugins(app: tauri::AppHandle, enabled: bool) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        let mut cfg = syncer::load_config(&paths)?
            .ok_or_else(|| anyhow::anyhow!("not configured yet"))?;
        cfg.sync_plugins = enabled;
        syncer::save_config(&paths, &cfg)?;
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Dev-only UI affordances (e.g. "Replay first launch") key off this.
#[tauri::command]
fn is_dev() -> bool {
    cfg!(debug_assertions)
}

#[tauri::command]
async fn sync_now(app: tauri::AppHandle) -> Result<syncer::SyncOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        syncer::sync_now(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn show_onboarding(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("onboarding") {
        let _ = w.show();
        let _ = w.center();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn close_onboarding(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("onboarding") {
        let _ = w.hide();
    }
}

#[tauri::command]
fn show_popover(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if TRAY_SEEN.load(Ordering::Relaxed) {
            let _ = win.move_window(Position::TrayBottomCenter);
        } else {
            let _ = win.center();
        }
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// Procedurally drawn template icon (black + alpha) matching the Material
/// "sync" glyph used in the onboarding logo, so the prototype needs no asset
/// pipeline. Geometry in the icon's 24pt space, scaled up. 3x3 supersampled.
fn tray_icon() -> Image<'static> {
    const S: usize = 44;
    let scale = S as f32 / 24.0;
    let c = 12.0 * scale;
    let r_in = 6.0 * scale;
    let r_out = 8.0 * scale;
    // Arrowhead: triangle (12,1) (12,9) tip (8,5), scaled — points left at top.
    let tri = [
        (12.0 * scale, 1.0 * scale),
        (12.0 * scale, 9.0 * scale),
        (8.0 * scale, 5.0 * scale),
    ];

    let sign = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| -> f32 {
        (p.0 - b.0) * (a.1 - b.1) - (a.0 - b.0) * (p.1 - b.1)
    };
    let in_tri = |p: (f32, f32)| -> bool {
        let d1 = sign(p, tri[0], tri[1]);
        let d2 = sign(p, tri[1], tri[2]);
        let d3 = sign(p, tri[2], tri[0]);
        let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(has_neg && has_pos)
    };
    let inside_one = |x: f32, y: f32| -> bool {
        let (dx, dy) = (x - c, y - c);
        let d = (dx * dx + dy * dy).sqrt();
        if d >= r_in && d <= r_out {
            // Ring segment from top (-90°) clockwise to +45°.
            let ang = dy.atan2(dx);
            if ang >= -std::f32::consts::FRAC_PI_2 && ang <= std::f32::consts::FRAC_PI_4 {
                return true;
            }
        }
        in_tri((x, y))
    };
    // Second arrow is the first rotated 180 degrees around the center.
    let inside = |x: f32, y: f32| -> bool {
        inside_one(x, y) || inside_one(2.0 * c - x, 2.0 * c - y)
    };

    let mut rgba = vec![0u8; S * S * 4];
    for yi in 0..S {
        for xi in 0..S {
            let mut hits = 0u32;
            for sy in 0..3 {
                for sx in 0..3 {
                    let x = xi as f32 + (sx as f32 + 0.5) / 3.0;
                    let y = yi as f32 + (sy as f32 + 0.5) / 3.0;
                    if inside(x, y) {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / 9) as u8;
            let o = (yi * S + xi) * 4;
            rgba[o + 3] = alpha; // black glyph, alpha = coverage (template image)
        }
    }
    Image::new_owned(rgba, S as u32, S as u32)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![
            quit_app,
            show_onboarding,
            close_onboarding,
            show_popover,
            detect_tools,
            engine_version,
            get_status,
            configure_default_store,
            sync_now,
            set_sync_plugins,
            is_dev
        ])
        .setup(|app| {
            // Menu bar app: no Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let window = app.get_webview_window("main").expect("main window");
            // Belt-and-suspenders against the startup flash: stay hidden until
            // the positioner places us under the tray icon on first open.
            let _ = window.hide();

            #[cfg(target_os = "macos")]
            {
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
                apply_vibrancy(
                    &window,
                    NSVisualEffectMaterial::Popover,
                    Some(NSVisualEffectState::Active),
                    Some(13.0),
                )
                .expect("failed to apply vibrancy");
            }

            TrayIconBuilder::with_id("main-tray")
                .icon(tray_icon())
                .icon_as_template(true)
                .tooltip("Code Sync")
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    TRAY_SEEN.store(true, Ordering::Relaxed);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("main") {
                            if win.is_visible().unwrap_or(false) {
                                let _ = win.hide();
                            } else {
                                let _ = win.move_window(Position::TrayBottomCenter);
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                // Popover behavior: dismiss when clicking elsewhere.
                tauri::WindowEvent::Focused(false) if window.label() == "main" => {
                    let _ = window.hide();
                }
                // Onboarding: closing hides so it can be reopened from Settings.
                tauri::WindowEvent::CloseRequested { api, .. }
                    if window.label() == "onboarding" =>
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
