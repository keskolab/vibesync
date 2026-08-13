//! Self-update via GitHub releases (adapted from the proven acom setup).
//!
//! Every release workflow run publishes platform bundles plus a signed
//! `latest.json` manifest; the app checks that manifest, verifies the
//! minisign signature against PUBKEY, and installs in place. The startup
//! check is notification-only — installing is always a user action in
//! Settings.

use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

#[derive(Default)]
pub struct PendingUpdate(pub Mutex<Option<Update>>);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub available: bool,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub notes: Option<String>,
    pub message: String,
}

/// Endpoint + pubkey live in tauri.conf.json (plugins.updater) — the plugin
/// refuses to initialize without them there. Env vars override for testing a
/// draft release against a temporary endpoint.
fn build_updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let ep = std::env::var("VIBESYNC_UPDATE_ENDPOINT").ok();
    let pk = std::env::var("VIBESYNC_UPDATE_PUBKEY").ok();
    if ep.is_none() && pk.is_none() {
        return app.updater().map_err(|e| e.to_string());
    }
    let mut b = app.updater_builder();
    if let Some(ep) = ep {
        let url: url::Url = ep.parse().map_err(|e| format!("update endpoint: {e}"))?;
        b = b.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    if let Some(pk) = pk {
        b = b.pubkey(pk);
    }
    b.build().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateCheckResult, String> {
    let current_version = app.package_info().version.to_string();
    let updater = build_updater(&app)?;
    let pending = app.state::<PendingUpdate>();
    match updater.check().await {
        Ok(Some(update)) => {
            let latest = update.version.clone();
            let notes = update.body.clone();
            *pending.0.lock().unwrap() = Some(update);
            Ok(UpdateCheckResult {
                available: true,
                current_version,
                latest_version: Some(latest.clone()),
                notes,
                message: format!("VibeSync {latest} is ready to install."),
            })
        }
        Ok(None) => {
            *pending.0.lock().unwrap() = None;
            Ok(UpdateCheckResult {
                available: false,
                current_version,
                latest_version: None,
                notes: None,
                message: "You're on the latest version.".to_string(),
            })
        }
        Err(e) => {
            *pending.0.lock().unwrap() = None;
            Err(format!("Update check failed: {e}"))
        }
    }
}

/// Download + install the update stashed by the last check, then relaunch.
#[tauri::command]
pub async fn install_pending_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .state::<PendingUpdate>()
        .0
        .lock()
        .unwrap()
        .take()
        .ok_or("No pending update — check for updates first.")?;
    update.download_and_install(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    relaunch(&app);
    Ok(())
}

/// Relaunch after an in-place update.
///
/// `AppHandle::restart` re-executes the CURRENT executable path, which on
/// macOS is the binary inside the .app the updater just replaced — the
/// running image is unlinked out from under it, so the app exited and never
/// came back and the user had to start it by hand. It was also called from a
/// spawned thread, where restart is not supported.
///
/// On macOS, hand the relaunch to Launch Services instead: `open -n` on the
/// bundle starts the NEW copy as its own process, and only then do we exit.
/// Other platforms replace the binary atomically and restart correctly.
fn relaunch(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        // …/VibeSync.app/Contents/MacOS/VibeSync -> …/VibeSync.app
        let bundle = std::env::current_exe().ok().and_then(|exe| {
            exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
        });
        if let Some(bundle) = bundle {
            if std::process::Command::new("open").arg("-n").arg(&bundle).spawn().is_ok() {
                // Give Launch Services a moment to pick up the new process
                // before this one disappears.
                std::thread::sleep(std::time::Duration::from_millis(500));
                app.exit(0);
                return;
            }
        }
    }
    app.restart();
}

/// Startup: silent check; on a hit, stash it, notify, and tell the popover.
pub async fn run_startup_update_check(app: AppHandle) {
    let Ok(updater) = build_updater(&app) else { return };
    // Launch-item startups race the network: the app is up before Wi-Fi
    // associates or the VPN connects, the single check failed, and nothing
    // ever retried — so an auto-started app could sit for days on an old
    // version while reporting nothing. Retry on ERROR only; a successful
    // "no update" answer is final.
    let mut update = None;
    for (attempt, delay) in [5u64, 30, 120].into_iter().enumerate() {
        match updater.check().await {
            Ok(Some(u)) => {
                update = Some(u);
                break;
            }
            Ok(None) => return, // definitively up to date
            Err(e) => {
                if let Ok(paths) = crate::syncer::paths(&app) {
                    crate::syncer::debug_log_event(
                        &paths,
                        &format!("update check attempt {} failed: {e}", attempt + 1),
                    );
                }
                tauri::async_runtime::spawn_blocking(move || {
                    std::thread::sleep(std::time::Duration::from_secs(delay))
                })
                .await
                .ok();
            }
        }
    }
    let Some(update) = update else { return };
    let latest = update.version.clone();
    let current = app.package_info().version.to_string();
    let notes = update.body.clone();
    *app.state::<PendingUpdate>().0.lock().unwrap() = Some(update);
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title("VibeSync update")
        .body(format!("Version {latest} is available — install it from Settings."))
        .show();
    let _ = app.emit(
        "updates://available",
        UpdateCheckResult {
            available: true,
            current_version: current,
            latest_version: Some(latest.clone()),
            notes,
            message: format!("VibeSync {latest} is ready to install."),
        },
    );
}
