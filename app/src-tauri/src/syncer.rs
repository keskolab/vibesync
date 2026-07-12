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
        sync_registry: true,
        disabled_tools: Vec::new(),
        disabled_scopes: Vec::new(),
        project_mappings: std::collections::BTreeMap::new(),
    })
}

#[derive(Debug, Serialize)]
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

    let ledger = load_ledger(paths);
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
                    id: CLAUDE_CODE.id,
                    name: CLAUDE_CODE.name,
                    installed,
                    enabled: claude_enabled,
                    sessions,
                    plans,
                    projects: claude_projects,
                    bytes,
                    agents: claude_agents,
                    skills: claude_skills,
                    last_activity_ms: claude_last,
                    new_items: 0,
                    new_ms: None,
                    new_seen: false,
                    new_sources: Default::default(),
                },
                ToolStatus {
                    id: "vscode",
                    name: "VS Code",
                    installed: engine::vscode::detect(),
                    enabled: enabled_for("vscode"),
                    sessions: vs_sessions,
                    plans: 0,
                    projects: vs_projects,
                    bytes: vs_bytes,
                    agents: 0,
                    skills: 0,
                    last_activity_ms: vs_last,
                    new_items: 0,
                    new_ms: None,
                    new_seen: false,
                    new_sources: Default::default(),
                },
                {
                    let (n, b, p, last) = engine::codex::light_counts(&home);
                    ToolStatus { id: "codex", name: "Codex", installed: engine::codex::detect(&home),
                        enabled: enabled_for("codex"), sessions: n, plans: 0, projects: p,
                        bytes: b, agents: 0, skills: 0, last_activity_ms: last,
                        new_items: 0, new_ms: None, new_seen: false, new_sources: Default::default() }
                },
                {
                    let (n, b, p, last) = engine::opencode::light_counts(&home);
                    ToolStatus { id: "opencode", name: "OpenCode", installed: engine::opencode::detect(&home),
                        enabled: enabled_for("opencode"), sessions: n, plans: 0, projects: p,
                        bytes: b, agents: 0, skills: 0, last_activity_ms: last,
                        new_items: 0, new_ms: None, new_seen: false, new_sources: Default::default() }
                },
                {
                    let (n, b, p) = engine::zed::light_counts();
                    ToolStatus { id: "zed", name: "Zed", installed: engine::zed::detect(),
                        enabled: enabled_for("zed"), sessions: n, plans: 0, projects: p,
                        bytes: b, agents: 0, skills: 0, last_activity_ms: None,
                        new_items: 0, new_ms: None, new_seen: false, new_sources: Default::default() }
                },
                {
                    let (n, b, last) = engine::copilot::light_counts(&home);
                    ToolStatus { id: "copilot", name: "Copilot CLI", installed: engine::copilot::detect(&home),
                        enabled: enabled_for("copilot"), sessions: n, plans: 0, projects: 0,
                        bytes: b, agents: 0, skills: 0, last_activity_ms: last,
                        new_items: 0, new_ms: None, new_seen: false, new_sources: Default::default() }
                },
            ]
        },
    };
    for t in &mut st.tools {
        if let Some(e) = ledger.0.get(t.id) {
            t.new_items = e.count;
            t.new_ms = Some(e.ts_ms);
            t.new_seen = e.seen;
            t.new_sources = e.sources.clone();
        }
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
    let mut config = load_config(paths)?.context("VibeSync is not configured yet")?;
    resolve_secrets(&mut config)?;
    let cache_dir = paths.config.parent().map(|p| p.to_path_buf());
    let store =
        engine::open_store_cached(&config.store, config.passphrase.as_deref(), cache_dir.as_deref())?;
    // Project identity mapping: learn `git origin -> local clone root` from
    // this machine's own sidebar entries, so the same repo cloned at
    // different paths on different machines still syncs as ONE project.
    let gitmap_path = paths.config.parent().unwrap().join("git_roots.json");
    let mut gitmap = engine::gitmap::GitMap::load(&gitmap_path);
    let mut gitmap_changed = false;
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
                        gitmap_changed |= gitmap.learn(std::path::Path::new(cwd));
                    }
                }
            }
        }
    }
    if gitmap_changed {
        let _ = gitmap.save(&gitmap_path);
    }
    let tok = engine::Tokenizer::from_env()?
        .with_gitmap(&gitmap)
        .with_manual_projects(&config.project_mappings);
    let home = dirs::home_dir().context("no home dir")?;

    let include_plugins = config.sync_plugins;
    let mut state = engine::SyncState::load(&paths.state)?;
    let on = |id: &str| !config.disabled_tools.iter().any(|t| t == id);
    // Never sync a tool onto a machine where it isn't installed — pulling
    // would create its data dirs and make the machine "become" an OpenCode/
    // Codex/... host it never was. Purge undetected tools from sync state so
    // installing the tool later re-pulls everything fresh.
    let claude_inst = home.join(".claude").is_dir();
    let vscode_inst = engine::vscode::detect();
    let codex_inst = engine::codex::detect(&home);
    let opencode_inst = engine::opencode::detect(&home);
    let zed_inst = engine::zed::detect();
    let copilot_inst = engine::copilot::detect(&home);
    for (inst, prefix) in [
        (claude_inst, "claude/"),
        (vscode_inst, "vscode/"),
        (codex_inst, "codex/"),
        (opencode_inst, "opencode/"),
        (zed_inst, "zed/"),
        (copilot_inst, "copilot/"),
    ] {
        if !inst {
            state.files.retain(|k, _| !k.starts_with(prefix));
        }
    }
    let claude_on = on("claude-code") && claude_inst;
    let vscode_on = on("vscode") && vscode_inst;
    let codex_on = on("codex") && codex_inst;
    let opencode_on = on("opencode") && opencode_inst;
    let zed_on = on("zed") && zed_inst;
    let copilot_on = on("copilot") && copilot_inst;
    // All config dirs: ~/.claude plus auto-detected ~/.claude-* profiles.
    let dirs = engine::adapters::Adapter::detect_config_dirs(&home);
    let mut entries = Vec::new();
    if claude_on {
        for dir in &dirs {
            entries.extend(CLAUDE_CODE.scan_dir(&home, dir, &tok, include_plugins)?);
        }
    }
    if vscode_on {
        entries.extend(engine::vscode::scan(&home)?);
    }
    if codex_on {
        entries.extend(engine::codex::scan(&home)?);
        state.mark_deletions(engine::codex::SESSIONS_PREFIX, &entries);
    }
    if opencode_on {
        entries.extend(engine::opencode::scan(&home)?);
        state.mark_deletions(engine::opencode::PREFIX, &entries);
    }
    if copilot_on {
        entries.extend(engine::copilot::scan(&home)?);
        state.mark_deletions(engine::copilot::PREFIX, &entries);
    }
    let shared_on = !config.disabled_scopes.iter().any(|s| s == "shared");
    if shared_on {
        entries.extend(engine::adapters::SHARED_SKILLS.scan(&home, &tok, false)?);
        state.mark_deletions("shared/skills", &entries);
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
    if vscode_on {
        state.mark_deletions("vscode/ws", &entries);
    }

    // ONE store listing for the whole sync — every pull below shares it.
    // (Each adapter used to list separately: 7+ full listings with per-object
    // meta fetches per sync.)
    let listing = store.list()?;
    // Unified progress space: pushed files + pull-side entries + registry.
    let total = entries.len() + listing.len() + 1;
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
    for chunk in entries.chunks(10) {
        let r = engine::sync::push(chunk, &mut state, store.as_ref(), &machine)?;
        push.pushed += r.pushed;
        push.unchanged += r.unchanged;
        let d = done.fetch_add(chunk.len(), std::sync::atomic::Ordering::Relaxed) + chunk.len();
        report_progress(d);
    }

    let mut pull = engine::Report::default();
    // Provenance: every pulled object's store meta names the machine that
    // uploaded it. Applies call record_pull(logical) on each real apply.
    let source_of: std::collections::HashMap<&str, &str> =
        listing.iter().map(|(l, m)| (l.as_str(), m.source.as_str())).collect();
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
        let Some(src) = source_of.get(logical) else { return };
        let src = canon_machine(src);
        if src == this_machine {
            return; // own uploads are not arrivals
        }
        let mut a = arrivals.lock().unwrap();
        *a.entry(tool).or_default().entry(src).or_default() += 1;
    };
    let record_registry = |logical: &str| {
        let Some(src) = source_of.get(logical) else { return };
        let src = canon_machine(src);
        if src == this_machine {
            return;
        }
        *registry_arrivals.lock().unwrap().entry(src).or_default() += 1;
    };
    if codex_on {
        let r = engine::codex::push_index(&home, &engine::machine_name(), &mut state, store.as_ref());
        let _ = r;
        if let Ok(r) = engine::codex::apply(&home, &mut state, store.as_ref(), &listing, &tick, &record_pull) {
            pull.pulled += r.pulled;
            pull.unchanged += r.unchanged;
            pull.skipped_newer_local += r.skipped_newer_local;
        }
    }
    if opencode_on {
        if let Ok(r) = engine::opencode::apply(&home, &mut state, store.as_ref(), &listing, &tick, &record_pull) {
            pull.pulled += r.pulled;
            pull.unchanged += r.unchanged;
            pull.skipped_newer_local += r.skipped_newer_local;
        }
        // db layer: modern OpenCode keeps sessions ONLY in opencode.db.
        let _ = engine::opencode::db_push(&home, &tok, &mut state, store.as_ref(), &engine::machine_name());
        if let Ok(r) = engine::opencode::db_apply(&home, &tok, &mut state, store.as_ref(), &listing, &tick, &record_pull) {
            pull.pulled += r.applied;
            pull.unchanged += r.unchanged;
            pull.skipped_newer_local += r.skipped_newer_local;
        }
    }
    if copilot_on {
        if let Ok(r) = engine::copilot::apply(&home, &mut state, store.as_ref(), &listing, &tick, &record_pull) {
            pull.pulled += r.pulled;
            pull.unchanged += r.unchanged;
            pull.skipped_newer_local += r.skipped_newer_local;
        }
    }
    if zed_on {
        // Zed rows are pushed here, not via the generic entry scan — its scan
        // yields db rows, not files. This call was missing entirely: no
        // machine ever published a thread, so the store's zed/ namespace
        // stayed empty fleet-wide.
        if let Ok(n) = engine::zed::push(&home, &mut state, store.as_ref(), &engine::machine_name()) {
            push.pushed += n;
        }
        if let Ok(r) = engine::zed::apply(&home, &mut state, store.as_ref(), &listing, &tick, &record_pull) {
            pull.pulled += r.applied;
            pull.unchanged += r.unchanged;
            pull.skipped_newer_local += r.skipped_newer_local;
        }
    }
    if vscode_on {
        let r = engine::vscode::apply(
            store.as_ref(),
            &mut state,
            &home,
            !config.disabled_scopes.iter().any(|s| s == "vscode-index"),
            &listing,
            &tick,
            &record_pull,
        )?;
        pull.pulled += r.applied;
        pull.unchanged += r.unchanged;
        pull.skipped_newer_local += r.skipped_newer_local;
    }
    if shared_on {
        let r = engine::sync::pull_dir(
            &engine::adapters::SHARED_SKILLS, &home, ".claude", &tok, &mut state,
            store.as_ref(), false, &|_| false, &listing, &tick, &record_pull,
        )?;
        pull.pulled += r.pulled;
        pull.unchanged += r.unchanged;
        pull.skipped_newer_local += r.skipped_newer_local;
    }
    for dir in dirs.iter().filter(|_| claude_on) {
        let r = engine::sync::pull_dir(
            &CLAUDE_CODE, &home, dir, &tok, &mut state, store.as_ref(), include_plugins,
            &|logical| scope_of(logical).map(|s| off.iter().any(|o| o == s)).unwrap_or(false),
            &listing, &tick, &record_pull,
        )?;
        pull.pulled += r.pulled;
        pull.skipped_newer_local += r.skipped_newer_local;
        pull.skipped_deleted += r.skipped_deleted;
        pull.unchanged += r.unchanged;
    }
    state.save(&paths.state)?;

    let (registry_pushed, registry_applied, registry_ghosts, registry_healed) =
        if config.sync_registry && claude_on {
            // Registry sync is best-effort — a failure must not fail the whole
            // sync — but it has to leave a trace: silently mapping errors to
            // zeros made a real Windows failure indistinguishable from
            // "nothing to do".
            let err_path = paths.config.parent().unwrap().join("registry_last_error.txt");
            match sync_registry(paths, store.as_ref(), &tok, &mut state, &listing, &record_registry) {
                Ok(r) => {
                    let _ = std::fs::remove_file(&err_path);
                    r
                }
                Err(e) => {
                    let _ = std::fs::write(&err_path, format!("{e:#}"));
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
    let mut ledger = NewLedger::default();
    let now = now_ms();
    for (id, sources) in arrivals {
        let count: usize = sources.values().sum();
        if count > 0 {
            ledger.0.insert(id.to_string(), NewEntry { count, ts_ms: now, seen: false, sources });
        }
    }
    let _ = save_ledger(paths, &ledger);

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

/// Which tool card a store path belongs to (for badges/provenance).
/// NOTE: new adapters must be added here too, or their arrivals silently get
/// no badge/provenance.
fn tool_of(logical: &str) -> Option<&'static str> {
    Some(match logical.split('/').next()? {
        "claude" => "claude-code",
        "vscode" => "vscode",
        "codex" => "codex",
        "opencode" => "opencode",
        "zed" => "zed",
        "copilot" => "copilot",
        "shared" => "shared",
        _ => return None,
    })
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

    // PUSH: tokenized, validated entries into the registry/ namespace.
    let mut pushed = 0usize;
    let scanned: Vec<String> = local.keys().map(|s| format!("claude/registry/{s}.json")).collect();
    let home = dirs::home_dir().context("no home dir")?;
    for (sid, (entry, path)) in &local {
        if registry::validate(entry).is_err() {
            continue; // never propagate a malformed entry
        }
        if !transcript_exists(entry, &home) {
            continue; // ghost entry — transcript deleted; would be unopenable
        }
        let mut out = entry.clone();
        registry::tokenize_paths(&mut out, tok);
        let bytes = serde_json::to_vec(&out)?;
        let hash = engine::scanner::hash_bytes(&bytes);
        let logical = format!("claude/registry/{sid}.json");
        if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
            continue;
        }
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
    let mut backed_up = false;
    for (logical, meta) in listing {
        let Some(sid) = logical.strip_prefix("claude/registry/").and_then(|s| s.strip_suffix(".json")) else {
            continue;
        };
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                continue;
            }
        }
        let Some((bytes, _)) = store.get(logical)? else { continue };
        let Ok(mut remote) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        registry::expand_paths(&mut remote, tok);
        registry::normalize_separators(&mut remote);

        let target = dir.join(format!("{sid}.json"));

        // Ghost guard: if the transcript isn't present locally, this entry
        // would be an unopenable sidebar item. Skip it — and if WE wrote it
        // on a previous sync, remove it (self-heal already-synced ghosts).
        if !transcript_exists(&remote, &home) {
            ghosts += 1;
            // We only ever wrote entries tracked in `applied`; a machine's own
            // native entries are never in that set, so this cannot delete them.
            if applied.contains(sid) && target.exists() {
                let _ = std::fs::remove_file(&target);
                applied.remove(sid);
                healed += 1;
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
                        continue;
                    }
                }
                remote
            }
        };
        // One-time safety net before the first write of this run.
        if !backed_up {
            let backup = paths.config.parent().unwrap().join("registry-backup");
            let _ = std::fs::create_dir_all(&backup);
            for (_, (_, p)) in &local {
                if let Some(name) = p.file_name() {
                    let _ = std::fs::copy(p, backup.join(name));
                }
            }
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
            on_pulled(logical);
        applied_count += 1;
        }
    }
    let _ = std::fs::write(&applied_path, serde_json::to_vec(&applied).unwrap_or_default());
    Ok((pushed, applied_count, ghosts, healed))
}
