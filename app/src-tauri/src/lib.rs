mod syncer;
mod updates;

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

/// Background autosync switch, mirrored from config at startup and toggled
/// from the UI. The worker thread polls it.
static AUTOSYNC: AtomicBool = AtomicBool::new(false);

const AUTOSYNC_INTERVAL_SECS: u64 = 15 * 60;

/// True while a sync runs — drives the tray spinner.
static SYNCING: AtomicBool = AtomicBool::new(false);

/// One icon swap per state change — animating the tray flickers on macOS,
/// so "syncing" is a static center-dot variant of the glyph instead.
fn set_tray_busy(app: &tauri::AppHandle, busy: bool) {
    SYNCING.store(busy, Ordering::SeqCst);
    if let Some(tray) = app.tray_by_id("main-tray") {
        let _ = tray.set_icon(Some(tray_icon_ex(busy)));
        let _ = tray.set_icon_as_template(true);
    }
}

/// The popover is on screen — the user is watching badges and counters
/// update live, so a toast would only repeat what they can already see.
fn popover_visible(app: &tauri::AppHandle) -> bool {
    app.get_webview_window("main")
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false)
}

/// Notify only when the sync brought NEWS the user isn't already looking
/// at: arrivals while the popover is closed. Uploads are this machine's
/// own routine work — never worth a toast (silence is health) — and when
/// a receiving app needs a restart to show the arrivals, the toast says
/// so instead of just counting.
fn notify_outcome(app: &tauri::AppHandle, outcome: &syncer::SyncOutcome) {
    use tauri_plugin_notification::NotificationExt;
    if outcome.pulled == 0 || popover_visible(app) {
        return;
    }
    let q = outcome.pulled;
    let mut body = format!("{q} new item{} arrived", if q == 1 { "" } else { "s" });
    if let Ok(paths) = syncer::paths(app) {
        let ledger = syncer::load_ledger(&paths);
        let names: Vec<&str> = ledger
            .0
            .iter()
            .filter(|(_, e)| e.restart_pid.is_some())
            .filter_map(|(id, _)| match id.as_str() {
                "claude-code" => Some("Claude Code"),
                "vscode" => Some("VS Code"),
                "zed" => Some("Zed"),
                _ => None,
            })
            .collect();
        if !names.is_empty() {
            body.push_str(&format!(" — restart {} to see them", names.join(" and ")));
        }
    }
    let _ = app.notification().builder().title("VibeSync").body(body).show();
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// First real engine call from the UI: which tools exist on this machine.
#[tauri::command]
fn detect_tools() -> Vec<vibesync_engine::adapters::DetectedTool> {
    vibesync_engine::adapters::detect_all()
}

/// True while any sync (manual or autosync) is running — lets a popover
/// opened mid-sync adopt the busy state instead of claiming Synced.
#[tauri::command]
fn is_syncing() -> bool {
    SYNCING.load(Ordering::SeqCst)
}

/// Generic UI trace: the frontend reports every user action (command name +
/// non-secret args). One line, any button — including future ones.
#[tauri::command]
fn log_ui(app: tauri::AppHandle, action: String) {
    if let Ok(paths) = syncer::paths(&app) {
        syncer::debug_log_event(&paths, &format!("ui: {action}"));
    }
}

#[tauri::command]
fn engine_version() -> &'static str {
    vibesync_engine::VERSION
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OnboardTool {
    id: &'static str,
    name: &'static str,
    installed: bool,
    /// Whether VibeSync can actually sync it yet (adapter built).
    supported: bool,
    sessions: usize,
}

/// Real detection for the setup assistant: probe each known tool's storage
/// with the same engine detects the syncer uses, so the two can't drift.
#[tauri::command]
fn detect_onboarding_tools() -> Vec<OnboardTool> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let count_jsonl = |rel: &str| -> usize {
        let mut n = 0;
        let mut stack = vec![home.join(rel)];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                        n += 1;
                    }
                }
            }
        }
        n
    };
    let exists = |rel: &str| home.join(rel).exists();
    vec![
        OnboardTool { id: "claude-code", name: "Claude Code", installed: exists(".claude/projects"), supported: true, sessions: count_jsonl(".claude/projects") },
        OnboardTool { id: "codex", name: "Codex", installed: vibesync_engine::codex::detect(&home), supported: true, sessions: count_jsonl(".codex/sessions") },
        OnboardTool { id: "opencode", name: "OpenCode", installed: vibesync_engine::opencode::detect(&home), supported: true, sessions: vibesync_engine::opencode::light_counts(&home).0 },
        OnboardTool { id: "zed", name: "Zed", installed: vibesync_engine::zed::detect(), supported: true, sessions: vibesync_engine::zed::light_counts().0 },
        OnboardTool { id: "copilot", name: "Copilot CLI", installed: vibesync_engine::copilot::detect(&home), supported: true, sessions: vibesync_engine::copilot::light_counts(&home).0 },
        OnboardTool { id: "vscode", name: "VS Code", installed: vibesync_engine::vscode::detect(), supported: true, sessions: vibesync_engine::vscode::light_counts().0 },
    ]
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

/// Clear a tool's "new items" badge — the user has viewed the tool.
#[tauri::command]
async fn ack_new(app: tauri::AppHandle, id: String) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        syncer::ack_new(&paths, &id)?;
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Create the global skills folder (agentskills.io spec) so shared skills can
/// live on this machine; returns refreshed status.
#[tauri::command]
async fn create_skills_dir(app: tauri::AppHandle) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        std::fs::create_dir_all(home.join(".agents/skills"))?;
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

/// Enable/disable syncing for a tool (consumed by sync_now and both UIs).
#[tauri::command]
async fn set_tool_enabled(
    app: tauri::AppHandle,
    id: String,
    enabled: bool,
) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        let mut cfg = syncer::load_config(&paths)?
            .ok_or_else(|| anyhow::anyhow!("not configured yet"))?;
        cfg.disabled_tools.retain(|t| t != &id);
        if !enabled {
            cfg.disabled_tools.push(id.clone());
        }
        syncer::save_config(&paths, &cfg)?;
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Toggle a sync scope. "plugins" and "registry" map to their dedicated
/// flags; the rest live in disabled_scopes.
#[tauri::command]
async fn set_scope_enabled(
    app: tauri::AppHandle,
    scope: String,
    enabled: bool,
) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = syncer::paths(&app)?;
        let mut cfg = syncer::load_config(&paths)?
            .ok_or_else(|| anyhow::anyhow!("not configured yet"))?;
        match scope.as_str() {
            "plugins" => cfg.sync_plugins = enabled,
            "registry" => cfg.sync_registry = enabled,
            _ => {
                cfg.disabled_scopes.retain(|s| s != &scope);
                if !enabled {
                    cfg.disabled_scopes.push(scope.clone());
                }
            }
        }
        syncer::save_config(&paths, &cfg)?;
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

/// Set the sync store (folder path, S3/R2 credentials, or Azure SAS URL)
/// plus optional encryption passphrase. The next milestone points the
/// onboarding UI here.
#[tauri::command]
async fn set_store(
    app: tauri::AppHandle,
    store: vibesync_engine::StoreConfig,
    passphrase: Option<String>,
) -> Result<syncer::Status, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Validate before saving: refuse configs the engine can't open.
        vibesync_engine::open_store(&store, passphrase.as_deref())?;
        let paths = syncer::paths(&app)?;
        let mut cfg = syncer::load_config(&paths)?.unwrap_or(syncer::default_config()?);
        // A different store means the sync state (what's already uploaded,
        // what was seen) belongs to the OLD store — reset it so the next
        // sync pushes everything to the new one. Compare non-secret identity
        // only: the saved config holds "@keychain" markers where the incoming
        // store has plaintext, so full JSON always differs and re-running
        // setup against the SAME store would wrongly reset state.
        fn identity(s: &vibesync_engine::StoreConfig) -> String {
            match s {
                vibesync_engine::StoreConfig::Folder { path, .. } => format!("folder:{path}"),
                vibesync_engine::StoreConfig::S3 { endpoint, region, bucket, access_key_id, .. } => {
                    format!("s3:{endpoint}:{region}:{bucket}:{access_key_id}")
                }
                vibesync_engine::StoreConfig::AzureSas { container_sas_url } => {
                    format!("azure:{}", container_sas_url.split('?').next().unwrap_or(""))
                }
            }
        }
        if identity(&cfg.store) != identity(&store) {
            let _ = std::fs::remove_file(&paths.state);
        }
        cfg.store = store;
        cfg.passphrase = passphrase;
        syncer::save_config(&paths, &cfg)?;
        syncer::status(&paths)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Native folder picker for the storage step.
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Open the store and LIST it — a real end-to-end connectivity check.
#[tauri::command]
async fn test_store(
    store: vibesync_engine::StoreConfig,
    passphrase: Option<String>,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let s = vibesync_engine::open_store(&store, passphrase.as_deref())?;
        Ok::<bool, anyhow::Error>(s.probe()?)
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
        use tauri::Emitter;
        let paths = syncer::paths(&app)?;
        set_tray_busy(&app, true);
        let emitter = app.clone();
        let result = syncer::sync_now(&paths, move |done, total| {
            let _ = emitter.emit("sync-progress", serde_json::json!({ "done": done, "total": total }));
        });
        set_tray_busy(&app, false);
        match &result {
            Ok(outcome) => notify_outcome(&app, outcome),
            Err(e) => syncer::debug_log_error(&paths, &format!("sync failed: {e:#}")),
        }
        result
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsState {
    autostart: bool,
    autosync: bool,
    autosync_interval_mins: u64,
    debug_logging: bool,
    project_mappings: std::collections::BTreeMap<String, String>,
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> SettingsState {
    use tauri_plugin_autostart::ManagerExt;
    let cfg = syncer::paths(&app)
        .ok()
        .and_then(|p| syncer::load_config(&p).ok().flatten());
    SettingsState {
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        autosync: AUTOSYNC.load(Ordering::Relaxed),
        autosync_interval_mins: cfg
            .as_ref()
            .map(|c| c.autosync_interval_mins)
            .unwrap_or(AUTOSYNC_INTERVAL_SECS / 60),
        debug_logging: cfg.as_ref().map(|c| c.debug_logging).unwrap_or(false),
        project_mappings: cfg.map(|c| c.project_mappings).unwrap_or_default(),
    }
}

/// Toggle debug.log (phase timings per sync) in the app data dir.
#[tauri::command]
fn set_debug_logging(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let paths = syncer::paths(&app).map_err(|e| e.to_string())?;
    let mut cfg = syncer::load_config(&paths)
        .map_err(|e| e.to_string())?
        .ok_or("VibeSync is not configured yet")?;
    cfg.debug_logging = enabled;
    syncer::save_config(&paths, &cfg).map_err(|e| e.to_string())?;
    if enabled {
        syncer::debug_log_banner(&paths);
    }
    Ok(())
}

/// Minutes between background syncs. The worker re-reads the config every
/// loop, so this applies without a restart.
#[tauri::command]
fn set_autosync_interval(app: tauri::AppHandle, mins: u64) -> Result<(), String> {
    let mins = mins.clamp(1, 24 * 60);
    let paths = syncer::paths(&app).map_err(|e| e.to_string())?;
    let mut cfg = syncer::load_config(&paths)
        .map_err(|e| e.to_string())?
        .ok_or("VibeSync is not configured yet")?;
    cfg.autosync_interval_mins = mins;
    syncer::save_config(&paths, &cfg).map_err(|e| e.to_string())
}

/// Add or update a manual project mapping (fleet name -> local folder).
#[tauri::command]
fn set_project_mapping(app: tauri::AppHandle, name: String, path: String) -> Result<SettingsState, String> {
    let name = name.trim().to_string();
    if !vibesync_engine::gitmap::valid_project_name(&name) {
        return Err("Project names: letters, digits, dot, dash, underscore (max 64).".into());
    }
    let path = path.trim().trim_end_matches(['/', '\\']).to_string();
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("Folder does not exist: {path}"));
    }
    let paths = syncer::paths(&app).map_err(|e| e.to_string())?;
    let mut cfg = syncer::load_config(&paths)
        .map_err(|e| e.to_string())?
        .ok_or("VibeSync is not configured yet")?;
    cfg.project_mappings.insert(name, path);
    syncer::save_config(&paths, &cfg).map_err(|e| e.to_string())?;
    Ok(get_settings(app))
}

#[tauri::command]
fn remove_project_mapping(app: tauri::AppHandle, name: String) -> Result<SettingsState, String> {
    let paths = syncer::paths(&app).map_err(|e| e.to_string())?;
    let mut cfg = syncer::load_config(&paths)
        .map_err(|e| e.to_string())?
        .ok_or("VibeSync is not configured yet")?;
    cfg.project_mappings.remove(&name);
    syncer::save_config(&paths, &cfg).map_err(|e| e.to_string())?;
    Ok(get_settings(app))
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let l = app.autolaunch();
    if enabled { l.enable() } else { l.disable() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn set_autosync(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    AUTOSYNC.store(enabled, Ordering::Relaxed);
    let paths = syncer::paths(&app).map_err(|e| e.to_string())?;
    if let Ok(Some(mut cfg)) = syncer::load_config(&paths) {
        cfg.autosync = enabled;
        syncer::save_config(&paths, &cfg).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Background worker: while autosync is on, sync every ~15 minutes and let an
/// open popover know so it can refresh.
fn spawn_autosync_worker(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        use tauri::Emitter;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            if !AUTOSYNC.load(Ordering::Relaxed) {
                continue;
            }
            // Don't stack onto a manual sync in flight.
            if SYNCING.load(Ordering::SeqCst) {
                continue;
            }
            let Ok(paths) = syncer::paths(&app) else { continue };
            let interval_secs = syncer::load_config(&paths)
                .ok()
                .flatten()
                .map(|c| c.autosync_interval_mins * 60)
                .unwrap_or(AUTOSYNC_INTERVAL_SECS);
            // Due-check on the WALL clock against the last real sync (the
            // state file's mtime — the same value "Last sync" displays, so
            // schedule and UI can't disagree). Instant is useless here: it
            // freezes during macOS sleep, silently stretching the 15-minute
            // interval into hours on laptops.
            let last_ms = std::fs::metadata(&paths.state)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if now_ms - last_ms < (interval_secs as i64) * 1000 {
                continue;
            }
            syncer::debug_log_event(&paths, "autosync: interval reached — starting sync");
            set_tray_busy(&app, true);
            // Tell an open popover the background sync started — the tray
            // icon alone isn't visible from inside the window.
            let _ = app.emit("autosync-start", ());
            let emitter = app.clone();
            // A panic inside a sync must not kill this thread — that would
            // silently disable autosync until the next app launch.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                syncer::sync_now(&paths, move |done, total| {
                    let _ = emitter
                        .emit("sync-progress", serde_json::json!({ "done": done, "total": total }));
                })
            }));
            set_tray_busy(&app, false);
            match result {
                Ok(Ok(outcome)) => {
                    notify_outcome(&app, &outcome);
                    let _ = app.emit("autosync-done", serde_json::json!(outcome));
                }
                Ok(Err(e)) => {
                    syncer::debug_log_error(&paths, &format!("autosync failed: {e:#}"));
                    // Failures matter enough to toast — but an open popover
                    // already shows the error via the event below.
                    if !popover_visible(&app) {
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app
                            .notification()
                            .builder()
                            .title("VibeSync")
                            .body(format!("Sync failed: {e:#}"))
                            .show();
                    }
                    let _ = app.emit("autosync-error", format!("{e:#}"));
                }
                Err(_) => {
                    if !popover_visible(&app) {
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app
                            .notification()
                            .builder()
                            .title("VibeSync")
                            .body("Sync crashed — will retry next interval")
                            .show();
                    }
                    let _ = app.emit("autosync-error", "sync crashed".to_string());
                }
            }
        }
    });
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

/// Place the popover: under the tray icon on macOS (menu bar on top),
/// bottom-right of the work area on Windows (taskbar at the bottom).
fn place_popover(win: &tauri::WebviewWindow) {
    if cfg!(target_os = "macos") {
        if TRAY_SEEN.load(Ordering::Relaxed) {
            let _ = win.move_window(Position::TrayBottomCenter);
        } else {
            let _ = win.center();
        }
        return;
    }
    let (Ok(Some(monitor)), Ok(size)) = (win.current_monitor(), win.outer_size()) else {
        let _ = win.center();
        return;
    };
    let wa = monitor.work_area();
    let margin = (12.0 * monitor.scale_factor()) as i32;
    let x = wa.position.x + wa.size.width as i32 - size.width as i32 - margin;
    let y = wa.position.y + wa.size.height as i32 - size.height as i32 - margin;
    let _ = win.set_position(tauri::PhysicalPosition { x, y });
}

#[tauri::command]
fn position_popover(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        place_popover(&win);
    }
}

/// Resize and re-anchor in one atomic step — two separate async calls race
/// on Windows (the bottom-anchored placement computed with a stale height).
#[tauri::command]
fn fit_popover(app: tauri::AppHandle, width: f64, height: f64) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_size(tauri::LogicalSize { width, height });
        if !cfg!(target_os = "macos") {
            place_popover(&win);
        }
    }
}

#[tauri::command]
fn show_popover(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        present_popover(&win);
    }
}

/// Place, show, and float the popover. Windows drops the topmost flag across
/// hide/show cycles (and can deny focus), which let other apps draw over the
/// open popover — so re-assert always-on-top on every show.
fn present_popover(win: &tauri::WebviewWindow) {
    use tauri::Emitter;
    place_popover(win);
    let _ = win.show();
    let _ = win.set_always_on_top(true);
    let _ = win.set_focus();
    // Hidden webviews can be suspended (macOS), missing autosync events —
    // so every show triggers a status re-fetch instead of trusting them.
    let _ = win.emit("popover-shown", ());
}

/// Procedurally drawn template icon (black + alpha) matching the Material
/// "sync" glyph used in the onboarding logo, so the prototype needs no asset
/// pipeline. Geometry in the icon's 24pt space, scaled up. 3x3 supersampled.
fn tray_icon() -> Image<'static> {
    tray_icon_ex(false)
}

/// `busy` adds a solid dot in the ring's center — the "syncing" state.
fn tray_icon_ex(busy: bool) -> Image<'static> {
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
        if busy {
            let (dx, dy) = (x - c, y - c);
            if dx * dx + dy * dy <= 28.0 {
                return true; // center activity dot
            }
        }
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
            // macOS: black template image (system tints it). Windows: white —
            // taskbars are dark and there is no auto-tinting.
            let shade: u8 = if cfg!(windows) { 255 } else { 0 };
            rgba[o] = shade;
            rgba[o + 1] = shade;
            rgba[o + 2] = shade;
            rgba[o + 3] = alpha;
        }
    }
    Image::new_owned(rgba, S as u32, S as u32)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(updates::PendingUpdate::default())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            quit_app,
            show_onboarding,
            close_onboarding,
            show_popover,
            detect_tools,
            detect_onboarding_tools,
            engine_version,
            log_ui,
            updates::check_for_updates,
            updates::install_pending_update,
            is_syncing,
            get_status,
            configure_default_store,
            sync_now,
            position_popover,
            fit_popover,
            set_sync_plugins,
            set_scope_enabled,
            create_skills_dir,
            ack_new,
            set_tool_enabled,
            is_dev,
            get_settings,
            set_autostart,
            set_autosync,
            set_autosync_interval,
            set_debug_logging,
            set_project_mapping,
            remove_project_mapping,
            set_store,
            pick_folder,
            test_store
        ])
        .setup(|app| {
            // Menu bar app: no Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Self-healing login item: the LaunchAgent records an absolute
            // executable path, which goes stale when the binary is renamed,
            // moved, or rebuilt elsewhere — launchd then resurrects an
            // ancient build as a frozen tray icon (live case: a two-day-old
            // `vibesync-app` started at login long after the rename to
            // `VibeSync`). Re-enabling rewrites the entry with the CURRENT
            // executable on every launch.
            {
                use tauri_plugin_autostart::ManagerExt;
                let l = app.autolaunch();
                if l.is_enabled().unwrap_or(false) {
                    let _ = l.enable();
                }
            }

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

            // Right-click menu; left click keeps toggling the popover.
            let menu = {
                use tauri::menu::{MenuBuilder, MenuItemBuilder};
                let open = MenuItemBuilder::with_id("open", "Open VibeSync").build(app)?;
                let quit = MenuItemBuilder::with_id("quit", "Quit VibeSync").build(app)?;
                MenuBuilder::new(app).item(&open).separator().item(&quit).build()?
            };
            TrayIconBuilder::with_id("main-tray")
                .icon(tray_icon())
                .icon_as_template(true)
                .tooltip("VibeSync")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => {
                        if let Some(win) = app.get_webview_window("main") {
                            present_popover(&win);
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
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
                                present_popover(&win);
                            }
                        }
                    }
                })
                .build(app)?;

            // Autosync: restore the persisted flag and start the worker.
            if let Ok(paths) = syncer::paths(app.handle()) {
                if let Ok(Some(cfg)) = syncer::load_config(&paths) {
                    AUTOSYNC.store(cfg.autosync, Ordering::Relaxed);
                }
            }
            if let Ok(paths) = syncer::paths(app.handle()) {
                syncer::debug_log_banner(&paths);
            }
            spawn_autosync_worker(app.handle().clone());
            tauri::async_runtime::spawn(updates::run_startup_update_check(app.handle().clone()));

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
