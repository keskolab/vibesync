//! App-side sync plumbing: config + state files in the app data dir, status
//! scans for the popover, and the actual engine-driven sync.

use std::path::PathBuf;

use anyhow::{Context, Result};
use vibesync_engine as engine;
use vibesync_engine::adapters::CLAUDE_CODE;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub store: engine::StoreConfig,
    /// Secret values live in the OS keychain (one entry holding all of
    /// them); this field then holds the "@keychain" marker. Plaintext is
    /// only a fallback when the keychain is unavailable — the file stays
    /// 0600 either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Plugins can be large — never synced unless the user opts in.
    #[serde(default)]
    pub sync_plugins: bool,
    /// Background sync while the app runs.
    #[serde(default)]
    pub autosync: bool,
    /// Minutes between background syncs.
    #[serde(default = "default_interval_mins")]
    pub autosync_interval_mins: u64,
    /// Write phase-by-phase timings to debug.log (Settings toggle).
    #[serde(default)]
    pub debug_logging: bool,
    /// Sync the Claude desktop app's sidebar registry (macOS).
    #[serde(default = "default_true")]
    pub sync_registry: bool,
    /// Tool ids the user has switched off (e.g. "claude-code").
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Scope ids switched off ("sessions", "plans", "config", "vscode-index").
    #[serde(default)]
    pub disabled_scopes: Vec<String>,
    /// Manual project mappings: fleet-wide project name -> this machine's
    /// local folder. Produces `${PROJ:name}` store tokens; outranks the
    /// automatic git-origin identity for the mapped folder.
    #[serde(default)]
    pub project_mappings: std::collections::BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

/// Per-tool "new items" ledger: holds what the MOST RECENT sync pulled in.
/// Every completed sync rewrites it, so a sync that brings nothing for a
/// tool clears its badge — "+N new" always describes the latest sync, never
/// a stale hour-old one. Viewing the tool clears its entry immediately.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NewLedger(pub std::collections::BTreeMap<String, NewEntry>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEntry {
    pub count: usize,
    pub ts_ms: i64,
    /// The user opened the tool since these arrived (badge hidden, but the
    /// provenance stays visible on the tool page until the next sync).
    #[serde(default)]
    pub seen: bool,
    /// Which machine each item came from: source -> count.
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, usize>,
    /// PID of the tool's GUI process that was running when these items
    /// arrived. That process read its session data at launch and won't see
    /// the new items until it restarts — the UI shows "restart X to see
    /// them" while this is set. None for CLI tools (they read fresh every
    /// run) or when the app wasn't running during the sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_pid: Option<u32>,
}

/// PID of the running GUI app a tool's synced data is invisible to until
/// restart. Only the apps that cache session data at launch are listed;
/// CLI tools always read fresh and return None. Multiple processes (VS
/// Code helpers, extra windows) resolve to the lowest pid — the main
/// process, which lives exactly as long as the app itself.
fn gui_pid(id: &str) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        // (-x = exact executable name, -f = full command line for names
        // too generic to match exactly, bound to the .app path. -a keeps
        // ancestors in the match — BSD pgrep silently drops them, which
        // hides the app entirely when the caller runs inside it.)
        let (exact, frag): (&[&str], &[&str]) = match id {
            "claude-code" => (&["Claude"], &[]),
            "vscode" => (&[], &["Visual Studio Code.app/", "Code - Insiders.app/"]),
            "zed" => (&["zed", "Zed"], &[]),
            _ => return None,
        };
        let mut pids: Vec<u32> = Vec::new();
        for (flag, pats) in [("-ax", exact), ("-af", frag)] {
            for p in pats {
                if let Ok(out) = std::process::Command::new("pgrep").arg(flag).arg(p).output() {
                    pids.extend(
                        String::from_utf8_lossy(&out.stdout)
                            .lines()
                            .filter_map(|l| l.trim().parse::<u32>().ok()),
                    );
                }
            }
        }
        pids.into_iter().min()
    }
    #[cfg(target_os = "windows")]
    {
        let names: &[&str] = match id {
            "claude-code" => &["Claude.exe"],
            "vscode" => &["Code.exe", "Code - Insiders.exe"],
            "zed" => &["Zed.exe"],
            _ => return None,
        };
        let mut pids: Vec<u32> = Vec::new();
        for n in names {
            if let Ok(out) = std::process::Command::new("tasklist")
                .args(["/FO", "CSV", "/NH", "/FI", &format!("IMAGENAME eq {n}")])
                .output()
            {
                // CSV rows: "Code.exe","1234",... — no-match prints an
                // INFO line that parses to nothing.
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some(pid) = line
                        .split("\",\"")
                        .nth(1)
                        .and_then(|s| s.trim_matches('"').parse().ok())
                    {
                        pids.push(pid);
                    }
                }
            }
        }
        pids.into_iter().min()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = id;
        None
    }
}

/// Rebuild the per-sync ledger. Fresh arrivals replace a tool's entry and
/// stamp the GUI pid that must restart before they're visible. An old
/// entry whose recorded process is STILL running carries forward — the
/// user hasn't restarted the app, so those items stay invisible no matter
/// how many quiet syncs pass, and the badge must not silently vanish.
fn rebuild_ledger(
    old: NewLedger,
    arrivals: std::collections::BTreeMap<&'static str, std::collections::BTreeMap<String, usize>>,
    now: i64,
    pid_of: &dyn Fn(&str) -> Option<u32>,
) -> NewLedger {
    let mut ledger = NewLedger::default();
    for (id, sources) in arrivals {
        let count: usize = sources.values().sum();
        if count > 0 {
            ledger.0.insert(
                id.to_string(),
                NewEntry { count, ts_ms: now, seen: false, sources, restart_pid: pid_of(id) },
            );
        }
    }
    for (id, e) in old.0 {
        if ledger.0.contains_key(&id) {
            continue;
        }
        if let Some(pid) = e.restart_pid {
            if pid_of(&id) == Some(pid) {
                ledger.0.insert(id, e);
            }
        }
    }
    ledger
}

fn ledger_path(paths: &Paths) -> PathBuf {
    paths.state.with_file_name("new_items.json")
}

pub fn load_ledger(paths: &Paths) -> NewLedger {
    std::fs::read(ledger_path(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_ledger(paths: &Paths, ledger: &NewLedger) -> Result<()> {
    Ok(std::fs::write(ledger_path(paths), serde_json::to_vec(ledger)?)?)
}

/// Per-tool count of items the last sync left waiting in storage because
/// their project isn't on this machine yet (repo not cloned, session
/// scaffolding not delivered). Rewritten every sync — parked items are
/// re-examined each pass, so the latest tally is the current truth.
fn parked_path(paths: &Paths) -> PathBuf {
    paths.state.with_file_name("parked.json")
}

pub fn load_parked(paths: &Paths) -> std::collections::BTreeMap<String, usize> {
    std::fs::read(parked_path(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_parked(paths: &Paths, parked: &std::collections::BTreeMap<String, usize>) -> Result<()> {
    Ok(std::fs::write(parked_path(paths), serde_json::to_vec(parked)?)?)
}

/// Mark a tool's badge seen (hides it; provenance stays on the tool page).
pub fn ack_new(paths: &Paths, id: &str) -> Result<()> {
    let mut ledger = load_ledger(paths);
    if let Some(e) = ledger.0.get_mut(id) {
        if !e.seen {
            e.seen = true;
            save_ledger(paths, &ledger)?;
        }
    }
    Ok(())
}

/// Troubleshooting log: every line goes through mini-log (its format, and
/// its LOG_LEVEL-gated console output for dev runs) and — when the Settings
/// toggle is on — is appended to debug.log next to the config file.
pub struct DebugLog(Option<std::sync::Arc<std::sync::Mutex<std::fs::File>>>);

impl DebugLog {
    pub fn open(paths: &Paths, enabled: bool) -> Self {
        if !enabled {
            engine::dlog::set_sink(None);
            return Self(None);
        }
        let p = debug_log_path(paths);
        // Verbose traces grow fast under 15-minute autosync: rotate at 5 MB
        // (previous trace survives as debug.log.old).
        if std::fs::metadata(&p).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
            let _ = std::fs::rename(&p, p.with_extension("log.old"));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
            .map(|f| std::sync::Arc::new(std::sync::Mutex::new(f)));
        // The engine does the actual file/network work — hand it the same
        // sink so its per-file trace lands in the same log, in order.
        if let Some(f) = &file {
            let sink = f.clone();
            engine::dlog::set_sink(Some(Box::new(move |line: &str| {
                use std::io::Write;
                let _ = writeln!(sink.lock().unwrap(), "{line}");
            })));
        }
        Self(file)
    }

    fn line(&self, level: mini_log::Level, msg: String) {
        // Skip building the message entirely when nothing would receive it.
        if self.0.is_none() && !mini_log::is_enabled(level) {
            return;
        }
        let line = mini_log::LogMessage::new(level, msg); // env-gated console
        if let Some(f) = &self.0 {
            use std::io::Write;
            let _ = writeln!(f.lock().unwrap(), "{line}");
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.line(mini_log::Level::Info, msg.into());
    }
    pub fn warn(&self, msg: impl Into<String>) {
        self.line(mini_log::Level::Warning, msg.into());
    }
    pub fn error(&self, msg: impl Into<String>) {
        self.line(mini_log::Level::Error, msg.into());
    }
}

pub fn debug_log_path(paths: &Paths) -> std::path::PathBuf {
    paths.config.parent().map(|p| p.join("debug.log")).unwrap_or_else(|| "debug.log".into())
}

/// For callers that see sync_now fail: record the error without needing the
/// DebugLog instance that died with it.
/// Visual delimiter marking a new logging session (app launch, or the
/// moment the toggle turns on) — written raw, without the per-line prefix,
/// so session boundaries jump out when scrolling a long log.
pub fn debug_log_banner(paths: &Paths) {
    let enabled = load_config(paths).ok().flatten().map(|c| c.debug_logging).unwrap_or(false);
    if !enabled {
        return;
    }
    let p = debug_log_path(paths);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        use std::io::Write;
        let _ = writeln!(
            f,
            "======================================\nNew Session Started — {}\n{} ({}) — VibeSync v{}\n======================================",
            mini_log::get_timestamp(),
            engine::machine_name(),
            std::env::consts::OS,
            env!("CARGO_PKG_VERSION"),
        );
    }
}

/// Settings changes are sync-relevant state: record them when logging is on.
pub fn debug_log_event(paths: &Paths, msg: &str) {
    let enabled = load_config(paths).ok().flatten().map(|c| c.debug_logging).unwrap_or(false);
    DebugLog::open(paths, enabled).info(msg);
}

pub fn debug_log_error(paths: &Paths, msg: &str) {
    let enabled = load_config(paths).ok().flatten().map(|c| c.debug_logging).unwrap_or(false);
    DebugLog::open(paths, enabled).error(msg);
}

/// Short human label for the configured store ("which storage").
fn store_label(store: &engine::StoreConfig) -> String {
    match store {
        engine::StoreConfig::Folder { path, .. } => format!("folder {path}"),
        engine::StoreConfig::S3 { endpoint, bucket, .. } => format!("s3 {bucket} @ {endpoint}"),
        engine::StoreConfig::AzureSas { .. } => "azure blob (SAS)".to_string(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn paths(app: &tauri::AppHandle) -> Result<Paths> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().context("resolve app data dir")?;
    std::fs::create_dir_all(&dir)?;
    Ok(Paths { config: dir.join("config.json"), state: dir.join("state.json") })
}

fn default_interval_mins() -> u64 {
    15
}

const KEYCHAIN_MARKER: &str = "@keychain";
const KEYRING_SERVICE: &str = "VibeSync";
const KEYRING_USER: &str = "store-secrets";

/// Every secret in one keychain entry: one unlock prompt per (dev) binary,
/// and the config file never holds live credentials.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    passphrase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    s3_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    azure_sas: Option<String>,
}

static SECRETS_CACHE: std::sync::Mutex<Option<Secrets>> = std::sync::Mutex::new(None);

fn keyring_entry() -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?)
}

fn read_secrets() -> Result<Secrets> {
    if let Some(s) = SECRETS_CACHE.lock().unwrap().clone() {
        return Ok(s);
    }
    let raw = keyring_entry()?.get_password()?;
    let secrets: Secrets = serde_json::from_str(&raw)?;
    *SECRETS_CACHE.lock().unwrap() = Some(secrets.clone());
    Ok(secrets)
}

fn write_secrets(secrets: &Secrets) -> Result<()> {
    keyring_entry()?.set_password(&serde_json::to_string(secrets)?)?;
    *SECRETS_CACHE.lock().unwrap() = Some(secrets.clone());
    Ok(())
}

/// Replace "@keychain" markers with the real values. Errors only when the
/// config references the keychain and it can't be read (locked/denied).
pub fn resolve_secrets(cfg: &mut AppConfig) -> Result<()> {
    let needs = cfg.passphrase.as_deref() == Some(KEYCHAIN_MARKER)
        || matches!(&cfg.store, engine::StoreConfig::S3 { secret_access_key, .. } if secret_access_key == KEYCHAIN_MARKER)
        || matches!(&cfg.store, engine::StoreConfig::AzureSas { container_sas_url } if container_sas_url == KEYCHAIN_MARKER);
    if !needs {
        return Ok(());
    }
    let secrets = read_secrets().context(
        "credentials are in the OS keychain but it could not be read — unlock the keychain and allow VibeSync",
    )?;
    if cfg.passphrase.as_deref() == Some(KEYCHAIN_MARKER) {
        cfg.passphrase = secrets.passphrase.clone();
    }
    match &mut cfg.store {
        engine::StoreConfig::S3 { secret_access_key, .. } if secret_access_key == KEYCHAIN_MARKER => {
            *secret_access_key = secrets.s3_secret.clone().unwrap_or_default();
        }
        engine::StoreConfig::AzureSas { container_sas_url } if container_sas_url == KEYCHAIN_MARKER => {
            *container_sas_url = secrets.azure_sas.clone().unwrap_or_default();
        }
        _ => {}
    }
    Ok(())
}

/// Move secret values out of `cfg` into the keychain, leaving markers.
/// Returns the scrubbed config; on keychain failure returns None (caller
/// falls back to plaintext-on-disk so nothing is ever lost).
fn scrub_secrets(cfg: &AppConfig) -> Option<AppConfig> {
    let mut secrets = Secrets::default();
    let mut scrubbed = cfg.clone();
    if let Some(p) = &cfg.passphrase {
        if p != KEYCHAIN_MARKER {
            secrets.passphrase = Some(p.clone());
            scrubbed.passphrase = Some(KEYCHAIN_MARKER.to_string());
        }
    }
    match &mut scrubbed.store {
        engine::StoreConfig::S3 { secret_access_key, .. } if secret_access_key != KEYCHAIN_MARKER => {
            secrets.s3_secret = Some(std::mem::replace(secret_access_key, KEYCHAIN_MARKER.to_string()));
        }
        engine::StoreConfig::AzureSas { container_sas_url } if container_sas_url != KEYCHAIN_MARKER => {
            secrets.azure_sas = Some(std::mem::replace(container_sas_url, KEYCHAIN_MARKER.to_string()));
        }
        _ => {}
    }
    if secrets.passphrase.is_none() && secrets.s3_secret.is_none() && secrets.azure_sas.is_none() {
        return Some(scrubbed); // nothing new to store (already markers / folder store)
    }
    // Merge with whatever is already in the keychain so a partial update
    // (e.g. only passphrase) keeps the other values.
    let mut merged = read_secrets().unwrap_or_default();
    if secrets.passphrase.is_some() {
        merged.passphrase = secrets.passphrase;
    }
    if secrets.s3_secret.is_some() {
        merged.s3_secret = secrets.s3_secret;
    }
    if secrets.azure_sas.is_some() {
        merged.azure_sas = secrets.azure_sas;
    }
    write_secrets(&merged).ok()?;
    Some(scrubbed)
}

/// The config holds credentials — owner-only, like ~/.aws/credentials.
fn restrict_perms(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path; // %APPDATA% is already per-user on Windows
}

pub fn load_config(paths: &Paths) -> Result<Option<AppConfig>> {
    if !paths.config.exists() {
        return Ok(None);
    }
    restrict_perms(&paths.config); // heal configs written by older builds
    let cfg: AppConfig = serde_json::from_slice(&std::fs::read(&paths.config)?)?;
    // One-time migration: plaintext secrets from older builds move to the
    // keychain (save_config scrubs). Skipped when already markers.
    let has_plain = cfg.passphrase.as_deref().map(|p| p != KEYCHAIN_MARKER).unwrap_or(false)
        || matches!(&cfg.store, engine::StoreConfig::S3 { secret_access_key, .. } if secret_access_key != KEYCHAIN_MARKER)
        || matches!(&cfg.store, engine::StoreConfig::AzureSas { container_sas_url } if container_sas_url != KEYCHAIN_MARKER);
    if has_plain {
        let _ = save_config(paths, &cfg);
    }
    Ok(Some(cfg)) // markers intact; resolve_secrets() when credentials needed
}

pub fn save_config(paths: &Paths, cfg: &AppConfig) -> Result<()> {
    // Prefer keychain-scrubbed; fall back to plaintext (0600) if the
    // keychain is unavailable rather than losing the credentials.
    let on_disk = scrub_secrets(cfg).unwrap_or_else(|| cfg.clone());
    std::fs::write(&paths.config, serde_json::to_vec_pretty(&on_disk)?)?;
    restrict_perms(&paths.config);
    Ok(())
}

/// Default first-run store: a plain local folder. Point `config.json` at an
/// iCloud Drive/Dropbox path or an S3 block to go multi-machine.
pub fn default_config() -> Result<AppConfig> {
    let home = dirs::home_dir().context("no home dir")?;
    Ok(AppConfig {
        store: engine::StoreConfig::Folder {
            path: home.join("VibeSyncStore").to_string_lossy().into_owned(),
            encrypted: false,
        },
        passphrase: None,
        sync_plugins: false,
        autosync: false,
        autosync_interval_mins: default_interval_mins(),
        debug_logging: false,
        sync_registry: true,
        disabled_tools: Vec::new(),
        disabled_scopes: Vec::new(),
        project_mappings: std::collections::BTreeMap::new(),
    })
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    pub enabled: bool,
    pub sessions: usize,
    pub plans: usize,
    pub projects: usize,
    pub bytes: u64,
    pub agents: usize,
    pub skills: usize,
    pub last_activity_ms: Option<i64>,
    /// Items the last sync pulled in for this tool.
    pub new_items: usize,
    pub new_ms: Option<i64>,
    /// Badge already viewed (hidden on the main page, shown on detail).
    pub new_seen: bool,
    /// Which machine the items came from: source -> count.
    pub new_sources: std::collections::BTreeMap<String, usize>,
    /// The tool's GUI was running when the items arrived and hasn't
    /// restarted since — it is still showing its launch-time view.
    pub needs_restart: bool,
    /// Items the last sync left waiting in storage — their project isn't
    /// on this machine yet, so they can't be placed anywhere.
    pub parked: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub configured: bool,
    pub store_desc: Option<String>,
    pub store_detail: Option<String>,
    pub last_sync_ms: Option<i64>,
    pub sync_plugins: bool,
    pub sync_registry: bool,
    pub disabled_scopes: Vec<String>,
    pub claude_enabled: bool,
    pub machine: String,
    pub shared_installed: bool,
    pub shared_enabled: bool,
    pub shared_skills: usize,
    pub shared_bytes: u64,
    pub shared_new: usize,
    pub shared_new_ms: Option<i64>,
    pub shared_new_seen: bool,
    pub shared_new_sources: std::collections::BTreeMap<String, usize>,
    pub tools: Vec<ToolStatus>,
}

/// Light scan: counts and sizes only — no hashing, safe on every popover open.
fn light_counts(home: &PathBuf, include_plugins: bool) -> (usize, usize, u64) {
    let mut sessions = 0usize;
    let mut plans = 0usize;
    let mut bytes = 0u64;
    let roots = CLAUDE_CODE
        .roots
        .iter()
        .chain(if include_plugins { CLAUDE_CODE.optional_roots.iter() } else { [].iter() });
    for root in roots {
        let mut abs = home.clone();
        for comp in root.home_rel.split('/') {
            abs.push(comp);
        }
        if !abs.exists() {
            continue;
        }
        if root.is_file {
            if let Ok(meta) = std::fs::metadata(&abs) {
                bytes += meta.len();
            }
            continue;
        }
        for entry in walkdir_files(&abs, root.exclude_dirs) {
            let ext_ok = root.exts.is_empty()
                || entry
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
                "projects" => {
                    if entry.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        sessions += 1;
                    }
                }
                _ => {}
            }
        }
    }
    (sessions, plans, bytes)
}

fn walkdir_files(root: &PathBuf, exclude_dirs: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    let skip = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| exclude_dirs.iter().any(|x| x.eq_ignore_ascii_case(n)))
                        .unwrap_or(false);
                    if !skip {
                        stack.push(path);
                    }
                }
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
    // Short, human description for the status line; the full detail goes in
    // the tooltip. Users think "my bucket on R2", not endpoints.
    let store_desc = config.as_ref().map(|c| match &c.store {
        engine::StoreConfig::Folder { path, encrypted } => {
            // Show the last few path components — enough to recognize the
            // folder without the full path.
            let comps: Vec<String> = std::path::Path::new(path)
                .components()
                .map(|p| p.as_os_str().to_string_lossy().into_owned())
                .collect();
            let tail = comps.len().saturating_sub(3);
            let shown = comps[tail..].join("/");
            let prefix = if tail > 1 { "\u{2026}/" } else { "" };
            format!("{prefix}{shown}{}", if *encrypted { " (encrypted)" } else { "" })
        }
        engine::StoreConfig::S3 { endpoint, .. } => {
            if endpoint.contains("r2.cloudflarestorage") {
                "R2 Cloudflare (encrypted)".to_string()
            } else {
                "Amazon S3 (encrypted)".to_string()
            }
        }
        engine::StoreConfig::AzureSas { .. } => "Azure Blob (encrypted)".to_string(),
    });
    let store_detail = config.as_ref().map(|c| match &c.store {
        engine::StoreConfig::Folder { path, .. } => path.clone(),
        engine::StoreConfig::S3 { bucket, endpoint, .. } => format!("{bucket} @ {endpoint}, encrypted client-side"),
        engine::StoreConfig::AzureSas { container_sas_url } => url::Url::parse(container_sas_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| format!("{h}, encrypted client-side")))
            .unwrap_or_default(),
    });
    let last_sync_ms = std::fs::metadata(&paths.state)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);

    let sync_plugins = config.as_ref().map(|c| c.sync_plugins).unwrap_or(false);
    let enabled_for = |id: &str| {
        config
            .as_ref()
            .map(|c| !c.disabled_tools.iter().any(|t| t == id))
            .unwrap_or(true)
    };
    let claude_enabled = enabled_for("claude-code");
    let enabled_for_scope = config
        .as_ref()
        .map(|c| !c.disabled_scopes.iter().any(|s| s == "shared"))
        .unwrap_or(true);
    let installed = CLAUDE_CODE.detect(&home);
    let (sessions, plans, bytes) =
        if installed { light_counts(&home, sync_plugins) } else { (0, 0, 0) };
    let claude_projects = std::fs::read_dir(home.join(".claude/projects"))
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    let count_files = |rel: &str| {
        walkdir_files(&home.join(rel), &[]).len()
    };
    let claude_agents = count_files(".claude/agents");
    let claude_skills = count_files(".claude/skills");
    let newest = |rel: &str, ext: Option<&str>| -> Option<i64> {
        walkdir_files(&home.join(rel), &[])
            .into_iter()
            .filter(|p| ext.is_none() || p.extension().and_then(|e| e.to_str()) == ext)
            .filter_map(|p| engine::scanner::mtime_ms(&p).ok())
            .max()
    };
    let claude_last = newest(".claude/projects", Some("jsonl"));

    let ledger = {
        // Resolve restart hints: a recorded pid that is no longer the
        // running process means the app restarted (or quit) and re-read
        // its data at launch — stop asking for a restart.
        let mut ledger = load_ledger(paths);
        let mut changed = false;
        for (id, e) in ledger.0.iter_mut() {
            if let Some(pid) = e.restart_pid {
                if gui_pid(id) != Some(pid) {
                    e.restart_pid = None;
                    changed = true;
                }
            }
        }
        if changed {
            let _ = save_ledger(paths, &ledger);
        }
        ledger
    };
    let mut st = Status {
        configured: config.is_some(),
        store_desc,
        store_detail,
        last_sync_ms,
        sync_plugins,
        sync_registry: config.as_ref().map(|c| c.sync_registry).unwrap_or(true),
        disabled_scopes: config.as_ref().map(|c| c.disabled_scopes.clone()).unwrap_or_default(),
        claude_enabled,
        machine: engine::machine_name(),
        shared_installed: home.join(".agents/skills").is_dir(),
        shared_enabled: enabled_for_scope,
        shared_skills: std::fs::read_dir(home.join(".agents/skills"))
            .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
            .unwrap_or(0),
        shared_bytes: walkdir_files(&home.join(".agents/skills"), &[])
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum(),
        shared_new: 0,
        shared_new_ms: None,
        shared_new_seen: false,
        shared_new_sources: Default::default(),
        tools: {
            let (vs_sessions, vs_bytes, vs_projects, vs_last) = engine::vscode::light_counts();
            vec![
                ToolStatus {
                    id: CLAUDE_CODE.id, name: CLAUDE_CODE.name, installed,
                    enabled: claude_enabled, sessions, plans, projects: claude_projects,
                    bytes, agents: claude_agents, skills: claude_skills,
                    last_activity_ms: claude_last, ..Default::default()
                },
                ToolStatus {
                    id: "vscode", name: "VS Code", installed: engine::vscode::detect(),
                    enabled: enabled_for("vscode"), sessions: vs_sessions,
                    projects: vs_projects, bytes: vs_bytes, last_activity_ms: vs_last,
                    ..Default::default()
                },
                {
                    let (n, b, p, last) = engine::codex::light_counts(&home);
                    ToolStatus { id: "codex", name: "Codex", installed: engine::codex::detect(&home),
                        enabled: enabled_for("codex"), sessions: n, projects: p, bytes: b,
                        last_activity_ms: last, ..Default::default() }
                },
                {
                    let (n, b, p, last) = engine::opencode::light_counts(&home);
                    ToolStatus { id: "opencode", name: "OpenCode", installed: engine::opencode::detect(&home),
                        enabled: enabled_for("opencode"), sessions: n, projects: p, bytes: b,
                        last_activity_ms: last, ..Default::default() }
                },
                {
                    let (n, b, p) = engine::zed::light_counts();
                    ToolStatus { id: "zed", name: "Zed", installed: engine::zed::detect(),
                        enabled: enabled_for("zed"), sessions: n, projects: p, bytes: b,
                        ..Default::default() }
                },
                {
                    let (n, b, last) = engine::copilot::light_counts(&home);
                    ToolStatus { id: "copilot", name: "Copilot CLI", installed: engine::copilot::detect(&home),
                        enabled: enabled_for("copilot"), sessions: n, bytes: b,
                        last_activity_ms: last, ..Default::default() }
                },
            ]
        },
    };
    let parked = load_parked(paths);
    for t in &mut st.tools {
        if let Some(e) = ledger.0.get(t.id) {
            t.new_items = e.count;
            t.new_ms = Some(e.ts_ms);
            t.new_seen = e.seen;
            t.new_sources = e.sources.clone();
            t.needs_restart = e.restart_pid.is_some();
        }
        t.parked = parked.get(t.id).copied().unwrap_or(0);
    }
    if let Some(e) = ledger.0.get("shared") {
        st.shared_new = e.count;
        st.shared_new_ms = Some(e.ts_ms);
        st.shared_new_seen = e.seen;
        st.shared_new_sources = e.sources.clone();
    }
    Ok(st)
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcome {
    pub pushed: usize,
    pub pulled: usize,
    pub registry_pushed: usize,
    pub registry_applied: usize,
    /// Expired remote entries skipped (their conversations were auto-deleted).
    pub registry_ghosts: usize,
    /// Previously-synced expired entries removed from this machine's sidebar.
    pub registry_healed: usize,
    pub skipped_newer_local: usize,
    pub skipped_deleted: usize,
    pub unchanged: usize,
}

/// `progress(done, total)` fires as push chunks complete; `total + 1` marks
/// the pull phase, and the final call is `(total + 1, total + 1)`.
pub fn sync_now(paths: &Paths, mut progress: impl FnMut(usize, usize) + Send) -> Result<SyncOutcome> {
    let sync_t0 = std::time::Instant::now();
    let _ = engine::scanner::take_hash_stats(); // reset for this sync's report
    if let Some(dir) = paths.config.parent() {
        engine::scanner::set_hash_cache_file(dir.join("hash_cache.json"));
    }
    let mut config = load_config(paths)?.context("VibeSync is not configured yet")?;
    let dlog = DebugLog::open(paths, config.debug_logging);
    dlog.info(format!("sync start — storage: {}", store_label(&config.store)));
    dlog.info("step: unlocking credentials");
    resolve_secrets(&mut config)?;
    let cache_dir = paths.config.parent().map(|p| p.to_path_buf());
    let t = std::time::Instant::now();
    let store =
        engine::open_store_cached(&config.store, config.passphrase.as_deref(), cache_dir.as_deref())?;
    dlog.info(format!("store opened in {} ms", t.elapsed().as_millis()));
    // Project identity mapping: learn `git origin -> local clone root` from
    // this machine's own sidebar entries, so the same repo cloned at
    // different paths on different machines still syncs as ONE project.
    let gitmap_path = paths.config.parent().unwrap().join("git_roots.json");
    let gitmap_t0 = std::time::Instant::now();
    let mut gitmap = engine::gitmap::GitMap::load(&gitmap_path);
    let mut gitmap_changed = false;
    let mut learned_cwds: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(dir) = registry_dir() {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("local_") || !name.ends_with(".json") {
                    continue;
                }
                let Ok(v) = std::fs::read(e.path())
                    .map_err(anyhow::Error::from)
                    .and_then(|b| Ok(serde_json::from_slice::<serde_json::Value>(&b)?))
                else {
                    continue;
                };
                for key in ["cwd", "originCwd"] {
                    if let Some(cwd) = v.get(key).and_then(|c| c.as_str()) {
                        if learned_cwds.insert(cwd.to_string()) {
                            gitmap_changed |= gitmap.learn(std::path::Path::new(cwd));
                        }
                    }
                }
            }
        }
    }
    // OpenCode anchors sessions to directories too — learn repo roots from
    // them so clones living at different paths per machine share one
    // ${GIT} identity, exactly like Claude sidebar cwds.
    if let Some(h) = dirs::home_dir() {
        for dir in engine::opencode::local_dirs(&h)
            .into_iter()
            .chain(engine::codex::local_dirs(&h))
            .chain(engine::vscode::local_dirs())
            .chain(engine::copilot::local_dirs(&h))
        {
            if learned_cwds.insert(dir.to_string_lossy().into_owned()) {
                gitmap_changed |= gitmap.learn(&dir);
            }
        }
    }
    if gitmap_changed {
        let _ = gitmap.save(&gitmap_path);
    }
    dlog.info(format!(
        "project map: {} git roots, {} manual mappings in {} ms",
        gitmap.roots.len(),
        config.project_mappings.len(),
        gitmap_t0.elapsed().as_millis()
    ));
    // Fleet atlas: every machine's clone location for every known repo,
    // shared through the store. Other machines' roots become tokenize-only
    // aliases, so a transcript recorded under a foreign clone path keys to
    // the one canonical ${GIT} identity — this is what merges duplicate
    // sidebar projects and retires legacy path-keyed store objects.
    let atlas = engine::atlas::sync_atlas(
        store.as_ref(),
        &gitmap,
        &dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default(),
        &engine::machine_name(),
    );
    // Clone discovery: a freshly `git clone`d repo has no sessions yet, so
    // nothing feeds it into the project map — and the sessions parked for it
    // in the store would wait forever. When the atlas knows identities this
    // machine hasn't mapped, probe the fleet's known locations (expanded
    // against this home) and the siblings of every known local root; any
    // directory whose own git origin matches a missing identity is learned
    // on the spot, and its parked sessions land this very sync.
    let missing: std::collections::HashSet<&str> = atlas
        .keys()
        .filter(|id| !gitmap.roots.contains_key(*id))
        .map(|s| s.as_str())
        .collect();
    if !missing.is_empty() {
        let home_s =
            dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default();
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        for id in &missing {
            for r in &atlas[*id] {
                let p = r
                    .strip_prefix("${HOME}")
                    .map(|rest| format!("{home_s}{rest}"))
                    .unwrap_or_else(|| r.clone());
                candidates.push(std::path::PathBuf::from(p));
            }
        }
        let parents: std::collections::HashSet<std::path::PathBuf> = gitmap
            .roots
            .values()
            .filter_map(|r| std::path::Path::new(r).parent().map(|p| p.to_path_buf()))
            .chain(candidates.iter().filter_map(|c| c.parent().map(|p| p.to_path_buf())))
            .collect();
        for parent in parents {
            if let Ok(rd) = std::fs::read_dir(&parent) {
                for e in rd.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        candidates.push(e.path());
                    }
                }
            }
        }
        let mut discovered = false;
        for c in candidates {
            if !c.is_dir() {
                continue;
            }
            // Only the directory ITSELF being a repo root counts — discover
            // walks up, and a plain subfolder must not learn its enclosing
            // repo here (that path isn't the clone root).
            if let Some((root, id)) = engine::gitmap::discover(&c) {
                if root == c && missing.contains(id.as_str()) {
                    discovered |= gitmap.learn(&c);
                }
            }
        }
        if discovered {
            let _ = gitmap.save(&gitmap_path);
        }
    }
    let tok = engine::Tokenizer::from_env()?
        .with_gitmap(&gitmap)
        .with_manual_projects(&config.project_mappings)
        .with_fleet_aliases(&atlas);
    let home = dirs::home_dir().context("no home dir")?;

    let include_plugins = config.sync_plugins;
    let mut state = engine::SyncState::load(&paths.state)?;
    let on = |id: &str| !config.disabled_tools.iter().any(|t| t == id);
    // Never sync a tool onto a machine where it isn't installed — pulling
    // would create its data dirs and make the machine "become" an OpenCode/
    // Codex/... host it never was. Purge undetected tools from sync state so
    // installing the tool later re-pulls everything fresh.
    dlog.info("step: detecting installed tools");
    let tools = tool_runs();
    // (installed, enabled) per table entry, table order.
    let tool_state: Vec<(bool, bool)> = tools
        .iter()
        .map(|t| {
            let inst = (t.detect)(&home);
            let enabled = match t.scope {
                Some(scope) => !config.disabled_scopes.iter().any(|s| s == scope),
                None => on(t.id),
            };
            (inst, enabled)
        })
        .collect();
    for (t, (inst, enabled)) in tools.iter().zip(&tool_state) {
        // Never sync a tool onto a machine where it isn't installed; purge
        // its sync state so a later install re-pulls fresh.
        if !inst {
            if let Some(prefix) = t.purge_prefix {
                state.files.retain(|k, _| !k.starts_with(prefix));
            }
        }
        dlog.info(format!(
            "tool {} ({}): installed={} switch={}{}",
            t.id,
            t.name,
            if *inst { "yes" } else { "no" },
            if *enabled { "on" } else { "OFF" },
            if *inst && *enabled { " -> syncing" } else { " -> skipped" }
        ));
        for p in (t.paths)(&home) {
            dlog.info(format!(
                "  location: {} ({})",
                p.display(),
                if p.exists() { "exists" } else { "missing" }
            ));
        }
    }
    dlog.info(format!(
        "settings: autosync={} every {} min, plugins={}, sidebar={}, disabled scopes={:?}",
        if config.autosync { "on" } else { "off" },
        config.autosync_interval_mins,
        if config.sync_plugins { "on" } else { "off" },
        if config.sync_registry { "on" } else { "off" },
        config.disabled_scopes
    ));
    let runs = |id: &str| {
        tools
            .iter()
            .zip(&tool_state)
            .find(|(t, _)| t.id == id)
            .map(|(_, (i, e))| *i && *e)
            .unwrap_or(false)
    };
    let claude_on = runs("claude-code");
    // All config dirs: ~/.claude plus auto-detected ~/.claude-* profiles.
    let dirs = engine::adapters::Adapter::detect_config_dirs(&home);
    dlog.info("step: scanning local files");
    let scan_t0 = std::time::Instant::now();
    let mut entries = Vec::new();
    let scan_env = ScanEnv { home: &home, dirs: &dirs, tok: &tok, include_plugins };
    for (t, (inst, enabled)) in tools.iter().zip(&tool_state) {
        let Some(scan) = t.scan else { continue };
        if !(*inst && *enabled) {
            continue;
        }
        let before = entries.len();
        let t0 = std::time::Instant::now();
        entries.extend(scan(&scan_env, &mut state)?);
        dlog.info(format!(
            "scan {}: {} files in {} ms",
            t.id,
            entries.len() - before,
            t0.elapsed().as_millis()
        ));
    }
    // Per-scope switches: drop disabled scopes from the push set.
    let off: Vec<String> = config.disabled_scopes.clone();
    entries.retain(|e| scope_of(&e.logical).map(|s| !off.iter().any(|o| o == s)).unwrap_or(true));
    if claude_on {
        for dir in &dirs {
            for prefix in CLAUDE_CODE.logical_prefixes(include_plugins) {
                let p = if dir == ".claude" {
                    format!("claude/{prefix}")
                } else {
                    format!("claude/profiles/{dir}/{prefix}")
                };
                state.mark_deletions(&p, &entries);
            }
        }
    }
    if runs("vscode") {
        state.mark_deletions("vscode/ws", &entries);
    }

    // ONE store listing for the whole sync — every pull below shares it.
    // (Each adapter used to list separately: 7+ full listings with per-object
    // meta fetches per sync.)
    dlog.info("step: listing the store");
    let t = std::time::Instant::now();
    let listing = store.list()?;
    dlog.info(format!(
        "scanned {} local files in {} ms; store listing: {} objects in {} ms",
        entries.len(),
        scan_t0.elapsed().as_millis(),
        listing.len(),
        t.elapsed().as_millis()
    ));
    {
        let hs = engine::scanner::take_hash_stats();
        dlog.info(format!(
            "hashing: {} files read ({:.1} MB) in {} ms, {} unchanged served from cache",
            hs.files_hashed,
            hs.bytes_hashed as f64 / (1024.0 * 1024.0),
            hs.hash_ms,
            hs.cache_hits
        ));
        if let Some((path, ms)) = hs.slowest {
            if ms > 250 {
                dlog.warn(format!(
                    "slowest file: {} took {ms} ms — repeated slow reads usually mean antivirus is scanning every file VibeSync opens",
                    path.display()
                ));
            }
        }
    }
    // Never overwrite a strictly newer store copy with older local bytes —
    // matters when fleet aliases re-key an old local copy of a file another
    // machine kept updating (mirror of the pull side's newer-local rule).
    {
        let store_view: std::collections::HashMap<&str, (&str, i64)> =
            listing.iter().map(|(k, m)| (k.as_str(), (m.hash.as_str(), m.mtime_ms))).collect();
        let before = entries.len();
        entries.retain(|e| {
            store_view
                .get(e.logical.as_str())
                .map(|(h, m)| *h == e.hash || *m <= e.mtime_ms)
                .unwrap_or(true)
        });
        let kept_back = before - entries.len();
        if kept_back > 0 {
            dlog.info(format!(
                "push: {kept_back} files kept back — the store has newer copies (they sync down instead)"
            ));
        }
    }
    // Unified progress space: scanned files plus the store objects of
    // ENABLED tools — disabled namespaces never tick, so counting them
    // left the button stuck far from its total.
    let enabled_ids: std::collections::HashSet<&str> = tools
        .iter()
        .zip(&tool_state)
        .filter(|(_, (inst, en))| *inst && *en)
        .map(|(t, _)| t.id)
        .collect();
    let total = entries.len()
        + listing
            .iter()
            .filter(|(k, _)| tool_of(k).map(|t| enabled_ids.contains(t)).unwrap_or(false))
            .count()
        + 1;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let progress_cell = std::sync::Mutex::new(&mut progress);
    let report_progress = |d: usize| {
        if let Ok(mut p) = progress_cell.lock() {
            p(d, total);
        }
    };
    let tick = || {
        let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        report_progress(d);
    };

    // Chunked push so the UI gets real progress.
    let machine = engine::machine_name();
    let mut push = engine::Report::default();
    let to_push_bytes: u64 = entries
        .iter()
        .filter(|e| state.files.get(&e.logical).map(|st| st.hash != e.hash).unwrap_or(true))
        .map(|e| e.size)
        .sum();
    dlog.info(format!("step: pushing changes ({:.1} MB to upload)", to_push_bytes as f64 / (1024.0 * 1024.0)));
    let push_t0 = std::time::Instant::now();
    for chunk in entries.chunks(10) {
        let r = engine::sync::push(chunk, &mut state, store.as_ref(), &machine)?;
        push.pushed += r.pushed;
        push.unchanged += r.unchanged;
        let d = done.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed) + chunk.len();
        report_progress(d);
    }

    {
        let ms = push_t0.elapsed().as_millis().max(1);
        let mbps = (to_push_bytes as f64 / (1024.0 * 1024.0)) / (ms as f64 / 1000.0);
        dlog.info(format!(
            "push: {} files ({:.1} MB) in {} ms — {:.1} MB/s up, {} unchanged",
            push.pushed,
            to_push_bytes as f64 / (1024.0 * 1024.0),
            ms,
            if push.pushed > 0 { mbps } else { 0.0 },
            push.unchanged
        ));
    }
    dlog.info("step: applying changes from other machines");
    let pull_t0 = std::time::Instant::now();
    let pulled_bytes = std::sync::atomic::AtomicU64::new(0);
    let mut pull = engine::Report::default();
    // Provenance: every pulled object's store meta names the machine that
    // uploaded it. Applies call record(logical) on each real apply.
    let source_of: std::collections::HashMap<&str, (&str, u64)> =
        listing.iter().map(|(l, m)| (l.as_str(), (m.source.as_str(), m.size))).collect();
    let this_machine = canon_machine(&engine::machine_name());
    let arrivals: std::sync::Mutex<
        std::collections::BTreeMap<&'static str, std::collections::BTreeMap<String, usize>>,
    > = Default::default();
    // Registry entries mirror transcripts: one arriving session produces BOTH
    // a transcript pull and a sidebar-entry apply. Counted separately, then
    // max-merged per source, so a session is one arrival — never two.
    let registry_arrivals: std::sync::Mutex<std::collections::BTreeMap<String, usize>> =
        Default::default();
    let record_pull = |logical: &str| {
        let Some(tool) = tool_of(logical) else { return };
        let Some((src, size)) = source_of.get(logical) else { return };
        pulled_bytes.fetch_add(*size, std::sync::atomic::Ordering::Relaxed);
        let src = canon_machine(src);
        if src == this_machine {
            return; // own uploads are not arrivals
        }
        let mut a = arrivals.lock().unwrap();
        *a.entry(tool).or_default().entry(src).or_default() += 1;
    };
    let record_registry = |logical: &str| {
        let Some((src, _)) = source_of.get(logical) else { return };
        let src = canon_machine(src);
        if src == this_machine {
            return;
        }
        *registry_arrivals.lock().unwrap().entry(src).or_default() += 1;
    };
    // ONE loop for every tool: publish, apply, count, log — identically.
    let apply_env = ApplyEnv {
        home: &home,
        dirs: &dirs,
        tok: &tok,
        include_plugins,
        off_scopes: &off,
        vscode_index: !config.disabled_scopes.iter().any(|s| s == "vscode-index"),
        machine: &machine,
        store: store.as_ref(),
        listing: &listing,
        tick: &tick,
        record: &record_pull,
    };
    let mut tool_errors: Vec<String> = Vec::new();
    let mut parked_counts: std::collections::BTreeMap<String, usize> = Default::default();
    for (t, (inst, enabled)) in tools.iter().zip(&tool_state) {
        if !(*inst && *enabled) {
            continue;
        }
        // Apply BEFORE publish: publish rewrites store objects and their
        // state records, and the listing snapshot predates it — applying
        // afterwards would read stale metas for objects we just replaced,
        // clobber the fresh state, and re-export the same content forever.
        let t0 = std::time::Instant::now();
        match (t.apply)(&apply_env, &mut state) {
            Err(e) => {
                dlog.error(format!("{}: apply failed: {e:#}", t.id));
                tool_errors.push(format!("{}: {e:#}", t.id));
            }
            Ok(r) => {
                if r.pulled > 0 {
                    dlog.info(format!(
                        "{}: {} applied, {} unchanged in {} ms",
                        t.id,
                        r.pulled,
                        r.unchanged,
                        t0.elapsed().as_millis()
                    ));
                }
                if r.parked > 0 {
                    dlog.warn(format!(
                        "{}: {} items parked (project not on this machine yet)",
                        t.id, r.parked
                    ));
                    parked_counts.insert(t.id.to_string(), r.parked);
                }
                for err in &r.errors {
                    dlog.error(format!("{}: {err}", t.id));
                    tool_errors.push(format!("{}: {err}", t.id));
                }
                pull.pulled += r.pulled;
                pull.unchanged += r.unchanged;
                pull.skipped_newer_local += r.skipped_newer_local;
                pull.skipped_deleted += r.skipped_deleted;
            }
        }
        if let Some(publish) = t.publish {
            match publish(&apply_env, &mut state) {
                Ok(n) => push.pushed += n,
                Err(e) => {
                    dlog.error(format!("{}: publish failed: {e:#}", t.id));
                    tool_errors.push(format!("{}: publish failed", t.id));
                }
            }
        }
    }
    {
        let ms = pull_t0.elapsed().as_millis().max(1);
        let down = pulled_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let mbps = (down as f64 / (1024.0 * 1024.0)) / (ms as f64 / 1000.0);
        dlog.info(format!(
            "pull: {} files ({:.1} MB) in {} ms — {:.1} MB/s down, {} unchanged",
            pull.pulled,
            down as f64 / (1024.0 * 1024.0),
            ms,
            if pull.pulled > 0 { mbps } else { 0.0 },
            pull.unchanged
        ));
        if pull.skipped_newer_local > 0 {
            dlog.warn(format!(
                "{} files were newer locally than in the store (kept local; store version skipped)",
                pull.skipped_newer_local
            ));
        }
    }
    state.save(&paths.state)?;

    let (registry_pushed, registry_applied, registry_ghosts, registry_healed) =
        if config.sync_registry && claude_on {
            // Registry sync is best-effort — a failure must not fail the whole
            // sync — but it has to leave a trace: silently mapping errors to
            // zeros made a real Windows failure indistinguishable from
            // "nothing to do".
            dlog.info("step: updating the Claude sidebar");
            if let Some(dir) = registry_dir() {
                dlog.info(format!("  sidebar registry: {}", dir.display()));
            }
            let err_path = paths.config.parent().unwrap().join("registry_last_error.txt");
            let t = std::time::Instant::now();
            match sync_registry(paths, store.as_ref(), &tok, &mut state, &listing, &record_registry) {
                Ok(r) => {
                    let _ = std::fs::remove_file(&err_path);
                    dlog.info(format!(
                        "claude sidebar: {} pushed, {} applied, {} ghosts skipped, {} healed in {} ms",
                        r.0,
                        r.1,
                        r.2,
                        r.3,
                        t.elapsed().as_millis()
                    ));
                    r
                }
                Err(e) => {
                    let _ = std::fs::write(&err_path, format!("{e:#}"));
                    dlog.error(format!("claude sidebar sync failed: {e:#}"));
                    (0, 0, 0, 0)
                }
            }
        } else {
            (0, 0, 0, 0)
        };
    report_progress(total);
    state.save(&paths.state)?;

    // Rewrite (not merge): the ledger reflects this sync only, clearing
    // badges for tools that received nothing.
    let mut arrivals = arrivals.into_inner().unwrap();
    // Fold sidebar-entry applies into claude-code as max per source: an
    // entry whose transcript also pulled this sync is the SAME session.
    let registry_arrivals = registry_arrivals.into_inner().unwrap();
    if !registry_arrivals.is_empty() {
        let claude = arrivals.entry("claude-code").or_default();
        for (src, n) in registry_arrivals {
            let e = claude.entry(src).or_default();
            *e = (*e).max(n);
        }
    }
    let ledger = rebuild_ledger(load_ledger(paths), arrivals, now_ms(), &gui_pid);
    let _ = save_ledger(paths, &ledger);
    let _ = save_parked(paths, &parked_counts);

    dlog.info(format!(
        "sync done in {} ms — {} up, {} down",
        sync_t0.elapsed().as_millis(),
        push.pushed,
        pull.pulled
    ));
    engine::scanner::save_hash_cache();
    engine::dlog::set_sink(None);

    // Partial failure: everything that could sync did (and is recorded), but
    // the outcome must say so instead of a green "Synced".
    if !tool_errors.is_empty() {
        anyhow::bail!("{} tool(s) failed to sync: {}", tool_errors.len(), tool_errors.join("; "));
    }

    Ok(SyncOutcome {
        pushed: push.pushed,
        pulled: pull.pulled,
        registry_pushed,
        registry_applied,
        registry_ghosts,
        registry_healed,
        skipped_newer_local: pull.skipped_newer_local,
        skipped_deleted: pull.skipped_deleted,
        unchanged: push.unchanged + pull.unchanged,
    })
}

/// Which user-facing scope a Claude logical path belongs to.
pub fn scope_of(logical: &str) -> Option<&'static str> {
    let rest = logical.strip_prefix("claude/")?;
    let rest = match rest.strip_prefix("profiles/") {
        Some(r) => r.splitn(2, '/').nth(1)?,
        None => rest,
    };
    Some(match rest.split('/').next()? {
        "projects" => "sessions",
        "plans" | "tasks" => "plans",
        "agents" | "skills" | "rules" => "config",
        "meta" => {
            if rest.ends_with("history.jsonl") { "plans" } else { "config" }
        }
        _ => return None, // plugins/registry have their own flags
    })
}

/// Hostnames flap between mDNS suffixes on macOS (Foo.local / Foo.lan /
/// Foo.home are the same machine on different networks) — compare and
/// aggregate provenance on the suffix-stripped name.
fn canon_machine(name: &str) -> String {
    let lower = name.to_lowercase();
    for suf in [".local", ".lan", ".home"] {
        if lower.ends_with(suf) {
            return name[..name.len() - suf.len()].to_string();
        }
    }
    name.to_string()
}

// ---------------------------------------------------------------- tools
//
// ONE table drives everything per-tool: detection (and state purge when a
// tool isn't installed), the enable switch, scanning, push-side publishing
// (db exports, index publish), and applying. Adding an adapter = adding one
// entry; the sync loop gives it the same tracing, timing, and error
// surfacing as every other tool automatically.

/// Store-namespace prefix -> tool id (badges, provenance, the table below).
const TOOL_PREFIXES: &[(&str, &str)] = &[
    ("claude", "claude-code"),
    ("vscode", "vscode"),
    ("codex", "codex"),
    ("opencode", "opencode"),
    ("zed", "zed"),
    ("copilot", "copilot"),
    ("shared", "shared"),
];

/// Which tool card a store path belongs to (for badges/provenance).
fn tool_of(logical: &str) -> Option<&'static str> {
    let ns = logical.split('/').next()?;
    TOOL_PREFIXES.iter().find(|(p, _)| *p == ns).map(|(_, id)| *id)
}

impl From<engine::sync::ApplyReport> for GenReport {
    fn from(r: engine::sync::ApplyReport) -> Self {
        GenReport {
            pulled: r.applied,
            unchanged: r.unchanged,
            skipped_newer_local: r.skipped_newer_local,
            parked: r.parked,
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
struct GenReport {
    pulled: usize,
    unchanged: usize,
    skipped_newer_local: usize,
    skipped_deleted: usize,
    parked: usize,
    /// Step-specific failures where the tool still made partial progress
    /// (e.g. opencode file layer ok, db merge failed). Logged per line and
    /// counted toward the sync's overall failure.
    errors: Vec<String>,
}

impl GenReport {
    fn absorb(&mut self, r: engine::sync::ApplyReport) {
        self.pulled += r.applied;
        self.unchanged += r.unchanged;
        self.skipped_newer_local += r.skipped_newer_local;
        self.parked += r.parked;
    }
}

struct ScanEnv<'a> {
    home: &'a std::path::Path,
    dirs: &'a [String],
    tok: &'a engine::Tokenizer,
    include_plugins: bool,
}

struct ApplyEnv<'a> {
    home: &'a std::path::Path,
    dirs: &'a [String],
    tok: &'a engine::Tokenizer,
    include_plugins: bool,
    off_scopes: &'a [String],
    vscode_index: bool,
    machine: &'a str,
    store: &'a dyn engine::SyncStore,
    listing: &'a [(String, engine::RemoteMeta)],
    tick: &'a (dyn Fn() + Sync),
    record: &'a (dyn Fn(&str) + Sync),
}

struct ToolRun {
    id: &'static str,
    name: &'static str,
    /// Purged from sync state when the tool isn't installed (None = never,
    /// e.g. shared skills are cross-tool by design).
    purge_prefix: Option<&'static str>,
    /// Enabled by a scope instead of a tool switch (shared skills).
    scope: Option<&'static str>,
    detect: fn(&std::path::Path) -> bool,
    /// Every location this tool reads/writes — logged with existence at each
    /// sync so "why isn't it detected on THIS machine" answers itself.
    paths: fn(&std::path::Path) -> Vec<std::path::PathBuf>,
    scan: Option<fn(&ScanEnv, &mut engine::SyncState) -> Result<Vec<engine::FileEntry>>>,
    publish: Option<fn(&ApplyEnv, &mut engine::SyncState) -> Result<usize>>,
    apply: fn(&ApplyEnv, &mut engine::SyncState) -> Result<GenReport>,
}

fn p_claude(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![home.join(".claude")]
}
fn p_vscode(_: &std::path::Path) -> Vec<std::path::PathBuf> {
    engine::vscode::storage_roots()
}
fn p_codex(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = vec![home.join(".codex/sessions"), home.join(".codex/session_index.jsonl")];
    v.push(engine::codex::state_db(home).unwrap_or_else(|| home.join(".codex/state_5.sqlite")));
    v
}
fn p_opencode(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    engine::opencode::probe_locations(home)
}
fn p_copilot(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![home.join(".copilot/session-state")]
}
fn p_zed(_: &std::path::Path) -> Vec<std::path::PathBuf> {
    engine::zed::probe_locations()
}
fn p_shared(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    vec![home.join(".agents/skills")]
}

fn d_claude(home: &std::path::Path) -> bool {
    home.join(".claude").is_dir()
}
fn d_vscode(_: &std::path::Path) -> bool {
    engine::vscode::detect()
}
fn d_zed(_: &std::path::Path) -> bool {
    engine::zed::detect()
}
fn d_always(_: &std::path::Path) -> bool {
    true
}

fn scan_claude(env: &ScanEnv, _state: &mut engine::SyncState) -> Result<Vec<engine::FileEntry>> {
    let mut out = Vec::new();
    for dir in env.dirs {
        out.extend(CLAUDE_CODE.scan_dir(env.home, dir, env.tok, env.include_plugins)?);
    }
    // Fleet aliases can map two local dirs to one logical key (the live
    // clone's transcripts plus a foreign-path copy materialized by pre-atlas
    // syncs): keep the newest copy of each.
    out.sort_by(|a, b| a.logical.cmp(&b.logical).then(b.mtime_ms.cmp(&a.mtime_ms)));
    out.dedup_by(|cur, prev| cur.logical == prev.logical);
    Ok(out)
}
fn scan_vscode(env: &ScanEnv, _state: &mut engine::SyncState) -> Result<Vec<engine::FileEntry>> {
    engine::vscode::scan(env.tok)
}
fn scan_opencode(env: &ScanEnv, state: &mut engine::SyncState) -> Result<Vec<engine::FileEntry>> {
    let out = engine::opencode::scan(env.home)?;
    state.mark_deletions(engine::opencode::PREFIX, &out);
    Ok(out)
}
fn scan_copilot(env: &ScanEnv, state: &mut engine::SyncState) -> Result<Vec<engine::FileEntry>> {
    let out = engine::copilot::scan(env.home)?;
    state.mark_deletions(engine::copilot::PREFIX, &out);
    Ok(out)
}
fn scan_shared(env: &ScanEnv, state: &mut engine::SyncState) -> Result<Vec<engine::FileEntry>> {
    let out = engine::adapters::SHARED_SKILLS.scan(env.home, env.tok, false)?;
    state.mark_deletions("shared/skills", &out);
    Ok(out)
}

fn publish_codex(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<usize> {
    // Session files carry tokenized content, so they need the dedicated
    // pusher; plus the legacy index for old builds, plus the thread-db
    // export that modern builds (state_<N>.sqlite) actually list from.
    let files = engine::codex::push_sessions(env.home, env.tok, state, env.store, env.machine, env.listing)?;
    engine::codex::push_index(env.home, env.machine, state, env.store)?;
    let threads = engine::codex::db_push(env.home, env.tok, state, env.store, env.machine, env.listing)?;
    Ok(files + threads)
}
fn publish_opencode(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<usize> {
    engine::opencode::db_push(env.home, env.tok, state, env.store, env.machine, env.listing)
}
fn publish_copilot(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<usize> {
    engine::copilot::db_push(env.home, env.tok, state, env.store, env.machine, env.listing)
}
fn publish_zed(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<usize> {
    engine::zed::push(env.home, state, env.store, env.machine, env.listing)
}

/// The `<dirtok>` of a `claude/projects/<dirtok>/...` (or profiles/...) key.
fn project_dirtok(logical: &str) -> Option<&str> {
    let rest = logical.strip_prefix("claude/")?;
    let rest = rest
        .strip_prefix("profiles/")
        .and_then(|r| r.split_once('/').map(|(_, r)| r))
        .unwrap_or(rest);
    let rest = rest.strip_prefix("projects/")?;
    Some(rest.split_once('/').map(|(d, _)| d).unwrap_or(rest))
}

/// A legacy path-keyed project dir this machine can canonicalize (via the
/// fleet atlas) is superseded — pulling it would materialize a duplicate
/// foreign-path project dir — but ONLY once the canonical key family
/// actually exists in the store; until then the legacy key is that
/// content's only home. Identity keys (${GIT}/${PROJ}) are never legacy:
/// a machine whose manual mapping outranks a git identity must still pull
/// the git-keyed objects. ${EHOME} fallback keys for unmapped folders pass
/// through untouched.
fn stale_claude_key(
    logical: &str,
    tok: &engine::Tokenizer,
    canon_present: &std::collections::HashSet<String>,
) -> bool {
    let Some(dirtok) = project_dirtok(logical) else { return false };
    if dirtok.starts_with("${GIT:") || dirtok.starts_with("${PROJ:") {
        return false;
    }
    let canon = tok.tokenize_encoded(&tok.expand_encoded(dirtok));
    canon != dirtok && canon_present.contains(&canon)
}

fn apply_claude(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    let mut g = GenReport::default();
    let canon_present: std::collections::HashSet<String> = env
        .listing
        .iter()
        .filter_map(|(k, _)| project_dirtok(k))
        .filter(|d| d.starts_with("${GIT:") || d.starts_with("${PROJ:"))
        .map(str::to_string)
        .collect();
    for dir in env.dirs {
        let r = engine::sync::pull_dir(
            &CLAUDE_CODE,
            env.home,
            dir,
            env.tok,
            state,
            env.store,
            env.include_plugins,
            &|logical| {
                if scope_of(logical)
                    .map(|s| env.off_scopes.iter().any(|o| o == s))
                    .unwrap_or(false)
                {
                    return true;
                }
                stale_claude_key(logical, env.tok, &canon_present)
            },
            env.listing,
            env.tick,
            env.record,
        )?;
        g.pulled += r.pulled;
        g.unchanged += r.unchanged;
        g.skipped_newer_local += r.skipped_newer_local;
        g.skipped_deleted += r.skipped_deleted;
        g.parked += r.parked;
    }
    Ok(g)
}
fn apply_vscode(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    Ok(engine::vscode::apply(
        env.store, state, env.tok, env.vscode_index, env.listing, env.tick, env.record,
    )?
    .into())
}
fn apply_codex(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    // File/index layer first so rollout files land before the thread-db
    // merge checks for them; the layers stay independent like OpenCode's.
    let mut g = GenReport::default();
    match engine::codex::apply(env.home, env.tok, state, env.store, env.listing, env.tick, env.record) {
        Ok(f) => g.absorb(f),
        Err(e) => g.errors.push(format!("file-layer apply failed: {e:#}")),
    }
    match engine::codex::db_apply(
        env.home, env.tok, state, env.store, env.listing, env.tick, env.record,
    ) {
        Ok(d) => g.absorb(d),
        Err(e) => g.errors.push(format!("thread-db apply failed: {e:#}")),
    }
    Ok(g)
}
fn apply_opencode(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    // The two layers are independent: a failure in one must not skip the
    // other or discard its counts.
    let mut g = GenReport::default();
    match engine::opencode::apply(env.home, state, env.store, env.listing, env.tick, env.record) {
        Ok(f) => g.absorb(f),
        Err(e) => g.errors.push(format!("file-layer apply failed: {e:#}")),
    }
    match engine::opencode::db_apply(
        env.home, env.tok, state, env.store, env.listing, env.tick, env.record,
    ) {
        Ok(d) => g.absorb(d),
        Err(e) => g.errors.push(format!("merging into opencode.db failed: {e:#}")),
    }
    Ok(g)
}
fn apply_copilot(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    // File layer first so session-state dirs land before the db merge
    // checks for them; the layers stay independent like Codex/OpenCode.
    let mut g = GenReport::default();
    match engine::copilot::apply(env.home, state, env.store, env.listing, env.tick, env.record) {
        Ok(f) => g.absorb(f),
        Err(e) => g.errors.push(format!("file-layer apply failed: {e:#}")),
    }
    match engine::copilot::db_apply(
        env.home, env.tok, state, env.store, env.listing, env.tick, env.record,
    ) {
        Ok(d) => g.absorb(d),
        Err(e) => g.errors.push(format!("merging into session-store.db failed: {e:#}")),
    }
    Ok(g)
}
fn apply_zed(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    Ok(engine::zed::apply(env.home, state, env.store, env.listing, env.tick, env.record)?.into())
}
fn apply_shared(env: &ApplyEnv, state: &mut engine::SyncState) -> Result<GenReport> {
    let r = engine::sync::pull_dir(
        &engine::adapters::SHARED_SKILLS,
        env.home,
        ".claude",
        env.tok,
        state,
        env.store,
        false,
        &|_| false,
        env.listing,
        env.tick,
        env.record,
    )?;
    Ok(GenReport {
        pulled: r.pulled,
        unchanged: r.unchanged,
        skipped_newer_local: r.skipped_newer_local,
        skipped_deleted: r.skipped_deleted,
        parked: r.parked,
        ..Default::default()
    })
}

/// The one list the sync loop walks — scan order and apply order alike.
fn tool_runs() -> Vec<ToolRun> {
    vec![
        ToolRun { id: "claude-code", name: "Claude Code", purge_prefix: Some("claude/"), scope: None, detect: d_claude, paths: p_claude, scan: Some(scan_claude), publish: None, apply: apply_claude },
        ToolRun { id: "vscode", name: "VS Code", purge_prefix: Some("vscode/"), scope: None, detect: d_vscode, paths: p_vscode, scan: Some(scan_vscode), publish: None, apply: apply_vscode },
        ToolRun { id: "codex", name: "Codex", purge_prefix: Some("codex/"), scope: None, detect: engine::codex::detect, paths: p_codex, scan: None, publish: Some(publish_codex), apply: apply_codex },
        ToolRun { id: "opencode", name: "OpenCode", purge_prefix: Some("opencode/"), scope: None, detect: engine::opencode::detect, paths: p_opencode, scan: Some(scan_opencode), publish: Some(publish_opencode), apply: apply_opencode },
        ToolRun { id: "copilot", name: "Copilot CLI", purge_prefix: Some("copilot/"), scope: None, detect: engine::copilot::detect, paths: p_copilot, scan: Some(scan_copilot), publish: Some(publish_copilot), apply: apply_copilot },
        ToolRun { id: "zed", name: "Zed", purge_prefix: Some("zed/"), scope: None, detect: d_zed, paths: p_zed, scan: None, publish: Some(publish_zed), apply: apply_zed },
        ToolRun { id: "shared", name: "Global skills", purge_prefix: None, scope: Some("shared"), detect: d_always, paths: p_shared, scan: Some(scan_shared), publish: None, apply: apply_shared },
    ]
}

// ---------------------------------------------------------------- registry

/// Locate the Claude desktop app's session registry dir:
/// - macOS:   ~/Library/Application Support/Claude/claude-code-sessions/<org>/<user>/
/// - Windows: %APPDATA%\Claude\claude-code-sessions\<org>\<user>\ for an
///   unpackaged install — but the Store (MSIX) build virtualizes %APPDATA%
///   into its package sandbox, invisible to outside processes, so also probe
///   %LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\... (that
///   physical path is exactly what the packaged app reads and writes).
fn registry_dir() -> Option<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(cfg) = dirs::config_dir() {
        bases.push(cfg.join("Claude").join("claude-code-sessions"));
    }
    #[cfg(windows)]
    {
        if let Some(local) = dirs::data_local_dir() {
            if let Ok(pkgs) = std::fs::read_dir(local.join("Packages")) {
                for p in pkgs.flatten() {
                    if p.file_name().to_string_lossy().starts_with("Claude_") {
                        bases.push(
                            p.path()
                                .join("LocalCache")
                                .join("Roaming")
                                .join("Claude")
                                .join("claude-code-sessions"),
                        );
                    }
                }
            }
        }
    }
    bases.iter().find_map(|b| first_org_user_dir(b))
}

/// Whether a Claude desktop installation exists on this machine at all
/// (config dir on macOS/unpackaged Windows, or an MSIX package dir).
fn claude_desktop_present() -> bool {
    if dirs::config_dir().map(|c| c.join("Claude").is_dir()).unwrap_or(false) {
        return true;
    }
    #[cfg(windows)]
    {
        if let Some(local) = dirs::data_local_dir() {
            if let Ok(pkgs) = std::fs::read_dir(local.join("Packages")) {
                return pkgs
                    .flatten()
                    .any(|p| p.file_name().to_string_lossy().starts_with("Claude_"));
            }
        }
    }
    false
}

/// First <org>/<user> directory two levels below `base`, if any.
fn first_org_user_dir(base: &std::path::Path) -> Option<PathBuf> {
    for org in std::fs::read_dir(base).ok()?.flatten() {
        if !org.path().is_dir() {
            continue;
        }
        for user in std::fs::read_dir(org.path()).ok()?.flatten() {
            if user.path().is_dir() {
                return Some(user.path());
            }
        }
    }
    None
}

/// The transcript a registry entry points at: <projects>/<encode(cwd)>/<cli>.jsonl.
/// Returns None if the entry has no cliSessionId or cwd.
fn transcript_path(entry: &serde_json::Value, home: &std::path::Path) -> Option<PathBuf> {
    let cli = entry.get("cliSessionId").and_then(|s| s.as_str())?;
    let cwd = entry.get("cwd").and_then(|s| s.as_str())?;
    let encoded = engine::tokenizer::encode_cwd(cwd);
    Some(home.join(".claude").join("projects").join(encoded).join(format!("{cli}.jsonl")))
}

fn transcript_exists(entry: &serde_json::Value, home: &std::path::Path) -> bool {
    transcript_path(entry, home).map(|p| p.exists()).unwrap_or(false)
}

/// One-time-per-run copy of every live registry entry before we modify any.
fn backup_registry(
    paths: &Paths,
    local: &std::collections::HashMap<String, (serde_json::Value, PathBuf)>,
) {
    let backup = paths.config.parent().unwrap().join("registry-backup");
    let _ = std::fs::create_dir_all(&backup);
    for (_, (_, p)) in local {
        if let Some(name) = p.file_name() {
            let _ = std::fs::copy(p, backup.join(name));
        }
    }
}

/// Validated, atomic, compact, 0600 write — every rule Gate A taught us.
fn write_registry_entry(path: &PathBuf, entry: &serde_json::Value) -> Result<()> {
    engine::registry::validate(entry)?;
    let bytes = serde_json::to_vec(entry)?; // compact, single line
    let tmp = path.with_extension("vibesync-tmp");
    std::fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Push local sidebar entries to the store and apply remote ones locally.
/// The registry is the highest-blast-radius surface in the product: one
/// malformed write blanks the user's entire session list, so every entry is
/// validated before it touches disk, and the first-ever write is preceded by
/// a full backup of the directory.
fn sync_registry(
    paths: &Paths,
    store: &dyn engine::SyncStore,
    tok: &engine::Tokenizer,
    state: &mut engine::SyncState,
    listing: &[(String, engine::RemoteMeta)],
    on_pulled: &dyn Fn(&str),
) -> Result<(usize, usize, usize, usize)> {
    use engine::registry;
    let Some(dir) = registry_dir() else {
        // CLI-only machine (no Claude desktop): nothing to sync, not an error
        // — bailing here would stamp registry_last_error.txt on every sync.
        if !claude_desktop_present() {
            return Ok((0, 0, 0, 0));
        }
        // Desktop IS installed but its sessions dir wasn't found: a real bug,
        // surface it instead of silently no-opping.
        let base = dirs::config_dir().map(|c| c.join("Claude").join("claude-code-sessions"));
        anyhow::bail!(
            "claude-code-sessions registry dir not found (base={:?}, exists={})",
            base,
            base.as_deref().map(|b| b.exists()).unwrap_or(false)
        );
    };

    // Session IDs we've written to the local registry from remote — lets us
    // safely remove ghost entries we created without ever touching the
    // machine's own native entries.
    let ghost_cache_path = paths.config.parent().unwrap().join("ghost_cache.json");
    let ghost_cache: std::collections::HashMap<String, (String, String)> =
        std::fs::read(&ghost_cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    let mut new_ghost_cache: std::collections::HashMap<String, (String, String)> =
        Default::default();
    // One-time repair: our own ghost-removals used to be recorded as user
    // deletions by the push sweep, permanently blocking those entries even
    // after their transcripts arrived. Clear registry tombstones once;
    // genuinely user-deleted entries resurrect one time and re-tombstone on
    // the next sweep if deleted again.
    let reset_marker = paths.config.parent().unwrap().join("tombstone_reset_v1");
    if !reset_marker.exists() {
        let cleared = {
            let before = state.files.len();
            state.files.retain(|k, st| !(k.starts_with("claude/registry/") && st.deleted_locally));
            before - state.files.len()
        };
        if cleared > 0 {
            engine::dlog::info(|| {
                format!("sidebar: cleared {cleared} stale deletion markers (one-time repair)")
            });
        }
        let _ = std::fs::write(&reset_marker, b"done");
    }
    let applied_path = paths.config.parent().unwrap().join("applied_registry.json");
    let mut applied: std::collections::HashSet<String> = std::fs::read(&applied_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    // Load all local entries.
    let mut local: std::collections::HashMap<String, (serde_json::Value, PathBuf)> =
        std::collections::HashMap::new();
    let mut cli_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in std::fs::read_dir(&dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("local_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(e.path())?) else {
            continue;
        };
        if let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) {
            if let Some(cli) = v.get("cliSessionId").and_then(|s| s.as_str()) {
                cli_ids.insert(cli.to_string());
            }
            local.insert(sid.to_string(), (v, e.path()));
        }
    }

    // HEAL: entries we created for other machines' sessions keep the cwd
    // they were born with; once the fleet atlas maps that path to a project
    // this machine has, snap the entry to the local clone. The transcript
    // moves WITH the entry: the copy at the old location is the one the user
    // has been appending to, so it is carried to the canonical location
    // unless a fresher file already lives there — re-pointing at a stale
    // pull would make recent turns vanish. Native entries are never touched.
    let home = dirs::home_dir().context("no home dir")?;
    let mut heals: Vec<(String, serde_json::Value)> = Vec::new();
    for (sid, (entry, _)) in &local {
        // Eligible: entries we created (applied set), plus any entry whose
        // cwd folder no longer exists — dead-path entries predate applied
        // tracking or come from a local folder move, and re-pointing them to
        // the project's current clone is precisely the heal (it is a
        // re-point, never a delete; the transcript travels along). Entries
        // whose folder still exists and aren't ours stay untouchable.
        if !applied.contains(sid.as_str()) {
            let cwd_gone = entry
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(|c| !std::path::Path::new(c).exists())
                .unwrap_or(false);
            if !cwd_gone {
                continue;
            }
        }
        let mut cand = entry.clone();
        if !registry::canonicalize_entry(&mut cand, tok) {
            continue;
        }
        let (Some(old_tp), Some(new_tp)) =
            (transcript_path(entry, &home), transcript_path(&cand, &home))
        else {
            continue;
        };
        let old_m = engine::scanner::mtime_ms(&old_tp).unwrap_or(0);
        let new_m = engine::scanner::mtime_ms(&new_tp).unwrap_or(0);
        if old_tp.exists() {
            if old_m == 0 {
                continue; // unreadable metadata — don't risk a stale re-point
            }
            // A transcript touched minutes ago may belong to a session that
            // is STILL RUNNING and appending to the old path; healing now
            // would strand everything typed after the copy. Wait for idle.
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(i64::MAX);
            if now_ms.saturating_sub(old_m) < 10 * 60 * 1000 {
                continue;
            }
            if !new_tp.exists() || old_m > new_m {
                let copied =
                    new_tp.parent().map(|p| std::fs::create_dir_all(p).is_ok()).unwrap_or(false)
                        && {
                            let tmp = new_tp.with_extension("vibesync-tmp");
                            std::fs::copy(&old_tp, &tmp).is_ok()
                                && std::fs::rename(&tmp, &new_tp).is_ok()
                        };
                if !copied {
                    continue;
                }
            }
        }
        if !new_tp.exists() {
            continue; // nothing to stand on yet — retry next sync
        }
        heals.push((sid.clone(), cand));
    }
    let mut backed_up = false;
    let mut repointed = 0usize;
    if !heals.is_empty() {
        backup_registry(paths, &local);
        backed_up = true;
    }
    for (sid, cand) in heals {
        let Some((entry, path)) = local.get_mut(&sid) else { continue };
        if write_registry_entry(path, &cand).is_ok() {
            *entry = cand;
            repointed += 1;
        }
    }
    if repointed > 0 {
        engine::dlog::info(|| {
            format!("sidebar: {repointed} synced entries re-pointed to this machine's project paths")
        });
    }

    // PUSH: tokenized, validated entries into the registry/ namespace.
    // Two guards learned from real Windows logs (2026-07-12), where idle
    // syncs re-uploaded 5-13 entries of ~180 KB each, every time:
    // - remoteMcpServersConfig is ~95% of an entry's bytes and is this
    //   machine's local MCP tool schemas — useless on another machine, so
    //   it is stripped from what we publish.
    // - Claude Desktop touches focus timestamps constantly; pushes are
    //   gated on a hash that ignores those fields, so an entry only
    //   re-uploads when something another machine would care about changed.
    const LOCAL_ONLY_FIELDS: &[&str] = &["remoteMcpServersConfig"];
    const VOLATILE_FIELDS: &[&str] = &["lastFocusedAt", "lastActivityAt", "promptSuggestion"];
    let gate_path = paths.config.parent().unwrap().join("registry_push_gate.json");
    let mut gate: std::collections::HashMap<String, String> = std::fs::read(&gate_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut pushed = 0usize;
    let scanned: Vec<String> = local.keys().map(|s| format!("claude/registry/{s}.json")).collect();
    for (sid, (entry, path)) in &local {
        if registry::validate(entry).is_err() {
            continue; // never propagate a malformed entry
        }
        if !transcript_exists(entry, &home) {
            continue; // ghost entry — transcript deleted; would be unopenable
        }
        let mut out = entry.clone();
        registry::tokenize_paths(&mut out, tok);
        if let Some(o) = out.as_object_mut() {
            for k in LOCAL_ONLY_FIELDS {
                o.remove(*k);
            }
        }
        let bytes = serde_json::to_vec(&out)?;
        let hash = engine::scanner::hash_bytes(&bytes);
        let logical = format!("claude/registry/{sid}.json");
        let mut stable = out.clone();
        if let Some(o) = stable.as_object_mut() {
            for k in VOLATILE_FIELDS {
                o.remove(*k);
            }
        }
        let gate_hash = engine::scanner::hash_bytes(&serde_json::to_vec(&stable)?);
        let unchanged_full = state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false);
        let unchanged_stable = gate.get(sid.as_str()).map(|h| *h == gate_hash).unwrap_or(false);
        if unchanged_full || unchanged_stable {
            gate.insert(sid.clone(), gate_hash);
            continue;
        }
        gate.insert(sid.clone(), gate_hash);
        let mtime = engine::scanner::mtime_ms(path).unwrap_or(0);
        store.put(
            &logical,
            &bytes,
            &engine::RemoteMeta { hash: hash.clone(), mtime_ms: mtime, size: bytes.len() as u64, source: engine::machine_name() },
        )?;
        state.files.insert(
            logical,
            engine::FileState { hash, mtime_ms: mtime, size: bytes.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    gate.retain(|sid, _| local.contains_key(sid));
    if let Ok(bytes) = serde_json::to_vec(&gate) {
        let _ = std::fs::write(&gate_path, bytes);
    }
    // Locally deleted entries must not resurrect.
    let present: std::collections::BTreeSet<&str> = scanned.iter().map(|s| s.as_str()).collect();
    for (logical, st) in state.files.iter_mut() {
        if logical.starts_with("claude/registry/") && !st.deleted_locally && !present.contains(logical.as_str()) {
            st.deleted_locally = true;
        }
    }

    // PULL: apply remote entries.
    let mut applied_count = 0usize;
    let mut ghosts = 0usize;
    let mut healed = 0usize;
    for (logical, meta) in listing {
        let Some(sid) = logical.strip_prefix("claude/registry/").and_then(|s| s.strip_suffix(".json")) else {
            continue;
        };
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                continue;
            }
        }
        // Known ghost, unchanged in the store, transcript still absent: skip
        // the fetch (these were re-downloaded every sync otherwise).
        if let Some((h, tpath)) = ghost_cache.get(logical.as_str()) {
            // Two marker kinds: a parked entry stores its tokenized cwd
            // (blocked while the token stays unexpandable); a plain ghost
            // stores its expected transcript path (blocked while absent).
            let still_blocked = if engine::gitmap::has_unresolved_token(tpath) {
                tok.expand_plain(tpath) == *tpath
            } else {
                !std::path::Path::new(tpath).exists()
            };
            if *h == meta.hash && still_blocked {
                ghosts += 1;
                new_ghost_cache.insert(logical.clone(), (h.clone(), tpath.clone()));
                continue;
            }
        }
        let Some((bytes, _)) = store.get(logical)? else { continue };
        let Ok(mut remote) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        registry::expand_paths(&mut remote, tok);
        registry::normalize_separators(&mut remote);

        // Parking rule (same as transcripts): an entry whose paths still
        // hold a token this machine can't expand belongs to a project that
        // isn't here yet. It must neither apply nor read as a ghost —
        // classifying it as one would delete a working local entry below.
        if registry::has_unresolved_paths(&remote) {
            ghosts += 1;
            // Cache marker = the tokenized cwd itself: the skip below holds
            // only while the token stays unexpandable, so cloning the repo
            // later re-evaluates this entry instead of parking it forever.
            if let Some(cwd) = remote.get("cwd").and_then(|c| c.as_str()) {
                new_ghost_cache.insert(logical.clone(), (meta.hash.clone(), cwd.to_string()));
            }
            continue;
        }

        let target = dir.join(format!("{sid}.json"));

        // Ghost guard: if the transcript isn't present locally, this entry
        // would be an unopenable sidebar item. Skip it — and if WE wrote it
        // on a previous sync, remove it (self-heal already-synced ghosts).
        if !transcript_exists(&remote, &home) {
            engine::dlog::debug(|| {
                format!(
                    "sidebar: skipping {sid} — transcript not on this machine ({})",
                    transcript_path(&remote, &home)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "unresolvable path".into())
                )
            });
            ghosts += 1;
            if let Some(tp) = transcript_path(&remote, &home) {
                new_ghost_cache
                    .insert(logical.clone(), (meta.hash.clone(), tp.display().to_string()));
            }
            // We only ever wrote entries tracked in `applied`; a machine's own
            // native entries are never in that set, so this cannot delete
            // them. And a local entry whose own transcript still opens is
            // never removed on the strength of the REMOTE's path view — that
            // view may simply not be materialized here yet.
            if applied.contains(sid) && target.exists() {
                let local_ok = local.get(sid).map(|(e, _)| transcript_exists(e, &home)).unwrap_or(false);
                if !local_ok {
                    let _ = std::fs::remove_file(&target);
                    applied.remove(sid);
                    // Drop the state entry too: the push-side deletion sweep
                    // must not read OUR removal as a user deletion — that
                    // tombstone would block the entry forever, even after
                    // its transcript arrives.
                    state.files.remove(logical);
                    healed += 1;
                }
            }
            continue;
        }
        let final_entry = match local.get(sid) {
            Some((local_entry, _)) => {
                let merged = registry::merge(local_entry, &remote);
                if &merged == local_entry {
                    // Nothing new; just record the store hash.
                    state.files.insert(
                        logical.clone(),
                        engine::FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                    );
                    continue;
                }
                merged
            }
            None => {
                // Brand-new entry: its cliSessionId must not collide with a
                // different existing entry (Gate A: collisions silently drop
                // BOTH entries from the sidebar).
                if let Some(cli) = remote.get("cliSessionId").and_then(|s| s.as_str()) {
                    if cli_ids.contains(cli) {
                        engine::dlog::warn(|| {
                            format!("sidebar: NOT applying {sid} — cliSessionId collision ({cli})")
                        });
                        continue;
                    }
                }
                remote
            }
        };
        // One-time safety net before the first write of this run.
        if !backed_up {
            backup_registry(paths, &local);
            backed_up = true;
        }
        if write_registry_entry(&target, &final_entry).is_ok() {
            if let Some(cli) = final_entry.get("cliSessionId").and_then(|s| s.as_str()) {
                cli_ids.insert(cli.to_string());
            }
            if !local.contains_key(sid) {
                applied.insert(sid.to_string());
            }
            state.files.insert(
                logical.clone(),
                engine::FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
            );
            engine::dlog::debug(|| format!("sidebar: applied entry {sid}"));
            on_pulled(logical);
            applied_count += 1;
        }
    }
    let _ = std::fs::write(&ghost_cache_path, serde_json::to_vec(&new_ghost_cache).unwrap_or_default());
    let _ = std::fs::write(&applied_path, serde_json::to_vec(&applied).unwrap_or_default());
    Ok((pushed, applied_count, ghosts, healed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_log_writes_mini_log_format() {
        let tmp = tempfile::tempdir().unwrap();
        let paths =
            Paths { config: tmp.path().join("config.json"), state: tmp.path().join("state.json") };
        let dlog = DebugLog::open(&paths, true);
        dlog.info("hello world");
        dlog.warn("watch out");
        dlog.error("boom");
        let text = std::fs::read_to_string(debug_log_path(&paths)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        // [<timestamp>] - LEVEL - message, e.g. [2026-05-30 - 09:30:42:10] - INFO - hello world
        assert!(lines[0].starts_with('['), "{}", lines[0]);
        assert!(lines[0].contains("] - INFO - hello world"), "{}", lines[0]);
        assert!(lines[1].contains("] - WARNING - watch out"), "{}", lines[1]);
        assert!(lines[2].contains("] - ERROR - boom"), "{}", lines[2]);
        // Engine trace lines route through the installed sink into the file.
        engine::dlog::debug(|| "engine trace line".to_string());
        let text = std::fs::read_to_string(debug_log_path(&paths)).unwrap();
        assert!(text.lines().count() >= 4, "{text}");
        assert!(text.contains("] - DEBUG - engine trace line"), "{text}");
        drop(dlog);

        // Disabled sink writes nothing new and uninstalls the engine sink.
        let off = DebugLog::open(&paths, false);
        off.info("should not appear");
        engine::dlog::debug(|| "should not appear either".to_string());
        let text2 = std::fs::read_to_string(debug_log_path(&paths)).unwrap();
        assert_eq!(text2.lines().count(), text.lines().count());
    }

    fn arrivals_of(
        id: &'static str,
        n: usize,
    ) -> std::collections::BTreeMap<&'static str, std::collections::BTreeMap<String, usize>> {
        let mut sources = std::collections::BTreeMap::new();
        sources.insert("other-machine".to_string(), n);
        let mut a = std::collections::BTreeMap::new();
        a.insert(id, sources);
        a
    }

    #[test]
    fn restart_hint_survives_quiet_syncs_until_the_app_restarts() {
        // Items arrive for VS Code while it's running (pid 42).
        let ledger = rebuild_ledger(NewLedger::default(), arrivals_of("vscode", 3), 1000, &|id| {
            (id == "vscode").then_some(42)
        });
        assert_eq!(ledger.0["vscode"].restart_pid, Some(42));
        assert_eq!(ledger.0["vscode"].count, 3);

        // A quiet sync while the same process is still running: the entry
        // carries forward — the app still shows its launch-time view.
        let ledger = rebuild_ledger(ledger, Default::default(), 2000, &|_| Some(42));
        assert_eq!(ledger.0["vscode"].count, 3, "badge must not vanish while stale");
        assert_eq!(ledger.0["vscode"].ts_ms, 1000, "original arrival time kept");

        // The app restarts (new pid): the next quiet sync drops the entry.
        let ledger = rebuild_ledger(ledger, Default::default(), 3000, &|_| Some(99));
        assert!(ledger.0.is_empty(), "restarted app has seen everything");
    }

    #[test]
    fn restart_hint_absent_for_cli_tools_and_closed_apps() {
        // CLI tool (pid lookup returns None) — no hint, and the entry
        // follows the original last-sync-only lifecycle.
        let ledger = rebuild_ledger(NewLedger::default(), arrivals_of("copilot", 2), 1000, &|_| None);
        assert_eq!(ledger.0["copilot"].restart_pid, None);
        let ledger = rebuild_ledger(ledger, Default::default(), 2000, &|_| None);
        assert!(ledger.0.is_empty(), "no restart pending: quiet sync clears as before");
    }

    #[test]
    fn fresh_arrivals_replace_a_pending_restart_entry() {
        let ledger = rebuild_ledger(NewLedger::default(), arrivals_of("vscode", 3), 1000, &|_| Some(42));
        // More items arrive in a later sync, same process still running:
        // fresh counts win, pid re-stamped.
        let ledger = rebuild_ledger(ledger, arrivals_of("vscode", 5), 2000, &|_| Some(42));
        assert_eq!(ledger.0["vscode"].count, 5);
        assert_eq!(ledger.0["vscode"].ts_ms, 2000);
        assert_eq!(ledger.0["vscode"].restart_pid, Some(42));
    }
}
