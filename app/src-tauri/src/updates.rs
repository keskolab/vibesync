//! Self-update via GitHub releases (adapted from the proven acom setup).
//!
//! Every release workflow run publishes platform bundles plus a signed
//! `latest.json` manifest; the app checks that manifest, verifies the
//! minisign signature against PUBKEY, and installs in place. The startup
//! check is notification-only — installing is always a user action in
//! Settings.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Fixed update source: the repo's latest release manifest. Overridable via
/// env for testing a draft release.
const UPDATE_ENDPOINT: &str =
    "https://github.com/JohnKesko/vibesync/releases/latest/download/latest.json";
/// Minisign public key matching the TAURI_SIGNING_PRIVATE_KEY repo secret.
const UPDATE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEUxREY5MDBEMEFBODU4MEYKUldRUFdLZ0tEWkRmNGJudDVFSnBlL1pmcnB5UXFHRy84aHhKdXk5Y3hzUC9qaUNnN1BMQVdybmQK";

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

fn build_updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let endpoint = std::env::var("VIBESYNC_UPDATE_ENDPOINT")
        .unwrap_or_else(|_| UPDATE_ENDPOINT.to_string());
    let url: url::Url = endpoint.parse().map_err(|e| format!("update endpoint: {e}"))?;
    app.updater_builder()
        .endpoints(vec![url])
        .map_err(|e| e.to_string())?
        .pubkey(std::env::var("VIBESYNC_UPDATE_PUBKEY").unwrap_or_else(|_| UPDATE_PUBKEY.to_string()))
        .build()
        .map_err(|e| e.to_string())
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
    let handle = app.clone();
    std::thread::spawn(move || handle.restart());
    Ok(())
}

/// Startup: silent check; on a hit, stash it, notify, and tell the popover.
pub async fn run_startup_update_check(app: AppHandle) {
    let Ok(updater) = build_updater(&app) else { return };
    let Ok(Some(update)) = updater.check().await else { return };
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
