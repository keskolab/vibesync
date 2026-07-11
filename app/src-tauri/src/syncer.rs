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
    /// M2 dev-only: plaintext in config.json. Moves to the OS keychain
    /// before anything ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// Plugins can be large — never synced unless the user opts in.
    #[serde(default)]
    pub sync_plugins: bool,
    /// Background sync every ~15 minutes while the app runs.
    #[serde(default)]
    pub autosync: bool,
    /// Sync the Claude desktop app's sidebar registry (macOS).
    #[serde(default = "default_true")]
    pub sync_registry: bool,
    /// Tool ids the user has switched off (e.g. "claude-code").
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

fn default_true() -> bool {
    true
}

pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
}

pub fn paths(app: &tauri::AppHandle) -> Result<Paths> {
    use tauri::Manager;
    let dir = app.path().app_data_dir().context("resolve app data dir")?;
    // One-time migration from the pre-rename identifier so existing
    // machines keep their config, state and applied-registry tracking.
    if !dir.exists() {
        if let Some(parent) = dir.parent() {
            let old = parent.join("com.keskolabs.codesync");
            if old.join("config.json").exists() {
                let _ = std::fs::rename(&old, &dir);
            }
        }
    }
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
            path: home.join("VibeSyncStore").to_string_lossy().into_owned(),
            encrypted: false,
        },
        passphrase: None,
        sync_plugins: false,
        autosync: false,
        sync_registry: true,
        disabled_tools: Vec::new(),
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
    pub store_detail: Option<String>,
    pub last_sync_ms: Option<i64>,
    pub sync_plugins: bool,
    pub claude_enabled: bool,
    pub machine: String,
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
    let claude_enabled = config
        .as_ref()
        .map(|c| !c.disabled_tools.iter().any(|t| t == "claude-code"))
        .unwrap_or(true);
    let installed = CLAUDE_CODE.detect(&home);
    let (sessions, plans, bytes) =
        if installed { light_counts(&home, sync_plugins) } else { (0, 0, 0) };

    Ok(Status {
        configured: config.is_some(),
        store_desc,
        store_detail,
        last_sync_ms,
        sync_plugins,
        claude_enabled,
        machine: engine::machine_name(),
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
pub fn sync_now(paths: &Paths, mut progress: impl FnMut(usize, usize)) -> Result<SyncOutcome> {
    let config = load_config(paths)?.context("VibeSync is not configured yet")?;
    let store = engine::open_store(&config.store, config.passphrase.as_deref())?;
    let tok = engine::Tokenizer::from_env()?;
    let home = dirs::home_dir().context("no home dir")?;

    if config.disabled_tools.iter().any(|t| t == "claude-code") {
        // The only adapter is switched off; nothing to do.
        return Ok(SyncOutcome::default());
    }
    let include_plugins = config.sync_plugins;
    let mut state = engine::SyncState::load(&paths.state)?;
    // All config dirs: ~/.claude plus auto-detected ~/.claude-* profiles.
    let dirs = engine::adapters::Adapter::detect_config_dirs(&home);
    let mut entries = Vec::new();
    for dir in &dirs {
        entries.extend(CLAUDE_CODE.scan_dir(&home, dir, &tok, include_plugins)?);
    }
    for dir in &dirs {
        for prefix in CLAUDE_CODE.logical_prefixes(include_plugins) {
            let p = if dir == ".claude" { prefix.to_string() } else { format!("profiles/{dir}/{prefix}") };
            state.mark_deletions(&p, &entries);
        }
    }

    // Chunked push so the UI gets real progress.
    let total = entries.len();
    let machine = engine::machine_name();
    let mut push = engine::Report::default();
    let mut done = 0usize;
    for chunk in entries.chunks(10) {
        let r = engine::sync::push(chunk, &mut state, store.as_ref(), &machine)?;
        push.pushed += r.pushed;
        push.unchanged += r.unchanged;
        done += chunk.len();
        progress(done, total + 1);
    }

    let mut pull = engine::Report::default();
    for dir in &dirs {
        let r = engine::sync::pull_dir(
            &CLAUDE_CODE, &home, dir, &tok, &mut state, store.as_ref(), include_plugins,
        )?;
        pull.pulled += r.pulled;
        pull.skipped_newer_local += r.skipped_newer_local;
        pull.skipped_deleted += r.skipped_deleted;
        pull.unchanged += r.unchanged;
    }
    state.save(&paths.state)?;
    progress(total + 1, total + 1);

    let (registry_pushed, registry_applied, registry_ghosts, registry_healed) =
        if config.sync_registry {
            sync_registry(paths, store.as_ref(), &tok, &mut state).unwrap_or((0, 0, 0, 0))
        } else {
            (0, 0, 0, 0)
        };
    state.save(&paths.state)?;

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

// ---------------------------------------------------------------- registry

/// Locate the Claude desktop app's session registry dir (macOS):
/// ~/Library/Application Support/Claude/claude-code-sessions/<org>/<user>/
fn registry_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None; // Windows location unknown until the app exists there
    }
    let base = dirs::home_dir()?
        .join("Library/Application Support/Claude/claude-code-sessions");
    for org in std::fs::read_dir(&base).ok()?.flatten() {
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
) -> Result<(usize, usize, usize, usize)> {
    use engine::registry;
    let Some(dir) = registry_dir() else { return Ok((0, 0, 0, 0)) };

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
    let scanned: Vec<String> = local.keys().map(|s| format!("registry/{s}.json")).collect();
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
        let logical = format!("registry/{sid}.json");
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
        if logical.starts_with("registry/") && !st.deleted_locally && !present.contains(logical.as_str()) {
            st.deleted_locally = true;
        }
    }

    // PULL: apply remote entries.
    let mut applied_count = 0usize;
    let mut ghosts = 0usize;
    let mut healed = 0usize;
    let mut backed_up = false;
    for (logical, meta) in store.list()? {
        let Some(sid) = logical.strip_prefix("registry/").and_then(|s| s.strip_suffix(".json")) else {
            continue;
        };
        if let Some(st) = state.files.get(&logical) {
            if st.deleted_locally || st.hash == meta.hash {
                continue;
            }
        }
        let Some((bytes, _)) = store.get(&logical)? else { continue };
        let Ok(mut remote) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        registry::expand_paths(&mut remote, tok);

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
            applied_count += 1;
        }
    }
    let _ = std::fs::write(&applied_path, serde_json::to_vec(&applied).unwrap_or_default());
    Ok((pushed, applied_count, ghosts, healed))
}
