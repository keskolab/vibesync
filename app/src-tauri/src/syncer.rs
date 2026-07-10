//! App-side sync plumbing: config + state files in the app data dir, status
//! scans for the popover, and the actual engine-driven sync.

use std::path::PathBuf;

use anyhow::{Context, Result};
use codesync_engine as engine;
use codesync_engine::adapters::CLAUDE_CODE;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub store: engine::StoreConfig,
    /// M2 dev-only: plaintext in config.json. Moves to the OS keychain
    /// before anything ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

pub fn paths(app: &tauri::AppHandle) -> Result<Paths> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().context("resolve app data dir")?;
    std::fs::create_dir_all(&dir)?;
    Ok(Paths { config: dir.join("config.json"), state: dir.join("state.json") })
}

pub fn load_config(paths: &Paths) -> Result<Option<AppConfig>> {
    if !paths.config.exists() {
        return Ok(None);
    }
    let cfg = serde_json::from_slice(&std::fs::read(&paths.config)?)?;
    Ok(Some(cfg))
}

pub fn save_config(paths: &Paths, cfg: &AppConfig) -> Result<()> {
    std::fs::write(&paths.config, serde_json::to_vec_pretty(cfg)?)?;
    Ok(())
}

/// Default first-run store: a plain local folder. Point `config.json` at an
/// iCloud Drive/Dropbox path or an S3 block to go multi-machine.
pub fn default_config() -> Result<AppConfig> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(AppConfig {
        store: engine::StoreConfig::Folder {
            path: home.join("CodeSyncStore").to_string_lossy().into_owned(),
            encrypted: false,
        },
        passphrase: None,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    pub sessions: usize,
    pub plans: usize,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub configured: bool,
    pub store_desc: Option<String>,
    pub last_sync_ms: Option<i64>,
    pub tools: Vec<ToolStatus>,
}

/// Light scan: counts and sizes only — no hashing, safe on every popover open.
fn light_counts(home: &PathBuf) -> (usize, usize, u64) {
    let mut sessions = 0usize;
    let mut plans = 0usize;
    let mut bytes = 0u64;
    for root in CLAUDE_CODE.roots {
        let mut abs = home.clone();
        for comp in root.home_rel.split('/') {
            abs.push(comp);
        }
        if !abs.exists() {
            continue;
        }
        for entry in walkdir_files(&abs) {
            let ext_ok = entry
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| root.exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&entry) {
                bytes += meta.len();
            }
            match root.logical_prefix {
                "plans" => plans += 1,
                _ => sessions += 1,
            }
        }
    }
    (sessions, plans, bytes)
}

fn walkdir_files(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    out
}

pub fn status(paths: &Paths) -> Result<Status> {
    let home = dirs::home_dir().context("no home dir")?;
    let config = load_config(paths)?;
    let store_desc = config.as_ref().map(|c| match &c.store {
        engine::StoreConfig::Folder { path, encrypted } => {
            format!("Folder: {path}{}", if *encrypted { " (encrypted)" } else { "" })
        }
        engine::StoreConfig::S3 { bucket, endpoint, .. } => {
            format!("Bucket: {bucket} @ {endpoint} (encrypted)")
        }
    });
    let last_sync_ms = std::fs::metadata(&paths.state)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);

    let installed = CLAUDE_CODE.detect(&home);
    let (sessions, plans, bytes) =
        if installed { light_counts(&home) } else { (0, 0, 0) };

    Ok(Status {
        configured: config.is_some(),
        store_desc,
        last_sync_ms,
        tools: vec![ToolStatus {
            id: CLAUDE_CODE.id,
            name: CLAUDE_CODE.name,
            installed,
            sessions,
            plans,
            bytes,
        }],
    })
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub pushed: usize,
    pub pulled: usize,
    pub skipped_newer_local: usize,
    pub skipped_deleted: usize,
    pub unchanged: usize,
}

pub fn sync_now(paths: &Paths) -> Result<SyncOutcome> {
    let config = load_config(paths)?.context("Code Sync is not configured yet")?;
    let store = engine::open_store(&config.store, config.passphrase.as_deref())?;
    let tok = engine::Tokenizer::from_env()?;
    let home = dirs::home_dir().context("no home dir")?;

    let mut state = engine::SyncState::load(&paths.state)?;
    let entries = CLAUDE_CODE.scan(&home, &tok)?;
    state.mark_deletions("projects", &entries);
    state.mark_deletions("plans", &entries);

    let push = engine::sync::push(&entries, &mut state, store.as_ref(), &engine::machine_name())?;
    let pull = engine::sync::pull(&CLAUDE_CODE, &home, &tok, &mut state, store.as_ref())?;
    state.save(&paths.state)?;

    Ok(SyncOutcome {
        pushed: push.pushed,
        pulled: pull.pulled,
        skipped_newer_local: pull.skipped_newer_local,
        skipped_deleted: pull.skipped_deleted,
        unchanged: push.unchanged + pull.unchanged,
    })
}
