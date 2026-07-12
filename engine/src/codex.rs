//! Codex adapter.
//!
//! Storage (validated on macOS, 2026-07):
//! - Sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` —
//!   date-partitioned, filename carries a unique uuid, so files never
//!   collide and sync purely additively. No home-dependent path components,
//!   nothing to tokenize.
//! - `~/.codex/session_index.jsonl` drives the Desktop app's session list
//!   (one JSON object per line: `{id, thread_name, updated_at}`), and it is
//!   incomplete relative to the files on disk — so listing is INDEX-DRIVEN.
//!
//! To make every machine's Codex list the union of all sessions, each
//! machine publishes its own index under `codex/index/<machine>.jsonl` and,
//! on apply, merges every machine's index (union by id, newest wins) into the
//! local one. Session files sync as plain content under `codex/sessions/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::dbsync::{expand_field, insert_map, query_maps, tokenize_field};
use crate::scanner::{hash_bytes, hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const SESSIONS_PREFIX: &str = "codex/sessions";
const INDEX_PREFIX: &str = "codex/index";

fn root(home: &Path) -> PathBuf {
    home.join(".codex")
}

/// Installed = ~/.codex holds something a real install writes (auth, config,
/// logs). Pre-gating VibeSync versions created sessions/ + the index on
/// machines without Codex — such residue must not read as an install.
pub fn detect(home: &Path) -> bool {
    let dir = root(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        crate::dlog::debug(|| format!("detect codex: NOT installed ({} missing)", dir.display()));
        return false;
    };
    let found = rd.flatten().any(|e| {
        let n = e.file_name();
        n != "sessions" && n != "session_index.jsonl"
    });
    crate::dlog::debug(|| {
        format!(
            "detect codex: {} ({} {})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            dir.display(),
            if found { "has real install files" } else { "" }
        )
    });
    found
}

/// Scan session files (index is handled separately on push/apply).
pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let dir = root(home).join("sessions");
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let rel = path
            .strip_prefix(&dir)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(FileEntry {
            logical: format!("{SESSIONS_PREFIX}/{rel}"),
            abs: path.to_path_buf(),
            size: entry.metadata()?.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

fn index_path(home: &Path) -> PathBuf {
    root(home).join("session_index.jsonl")
}

/// Parse a JSONL index into id -> (updated_at, raw line).
fn parse_index(bytes: &[u8]) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let updated =
                    v.get("updated_at").and_then(|x| x.as_str()).unwrap_or("").to_string();
                map.insert(id.to_string(), (updated, line.to_string()));
            }
        }
    }
    map
}

/// Push session files (via the generic pusher, caller-supplied entries) is
/// handled by the caller; here we publish this machine's index object.
pub fn push_index(
    home: &Path,
    machine: &str,
    state: &mut SyncState,
    store: &dyn SyncStore,
) -> Result<()> {
    let path = index_path(home);
    if !path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(&path)?;
    let hash = crate::scanner::hash_bytes(&bytes);
    let logical = format!("{INDEX_PREFIX}/{}.jsonl", sanitize(machine));
    if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
        return Ok(());
    }
    let mtime = mtime_ms(&path).unwrap_or(0);
    store.put(
        &logical,
        &bytes,
        &RemoteMeta { hash: hash.clone(), mtime_ms: mtime, size: bytes.len() as u64, source: machine.to_string() },
    )?;
    state.files.insert(
        logical,
        FileState { hash, mtime_ms: mtime, size: bytes.len() as u64, deleted_locally: false },
    );
    Ok(())
}

/// Apply session files this machine lacks, then union every machine's index
/// into the local session_index.jsonl (never dropping local entries).
pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let sessions_root = root(home).join("sessions");
    let index_prefix = format!("{INDEX_PREFIX}/");
    let session_prefix = format!("{SESSIONS_PREFIX}/");

    // Merge starts from the local index so we never lose local sessions.
    let mut merged = std::fs::read(index_path(home))
        .map(|b| parse_index(&b))
        .unwrap_or_default();

    for (logical, meta) in listing {
        // Index objects: union in.
        if let Some(_machine) = logical.strip_prefix(&index_prefix) {
            on_file();
            if let Some((bytes, _)) = store.get(logical)? {
                for (id, (updated, line)) in parse_index(&bytes) {
                    match merged.get(&id) {
                        Some((cur, _)) if *cur >= updated => {}
                        _ => {
                            if merged.insert(id.clone(), (updated, line)).is_none() {
                                crate::dlog::debug(|| {
                                    format!("codex: merged session {id} into local index")
                                });
                            }
                        }
                    }
                }
            }
            continue;
        }
        // Session files.
        let Some(rest) = logical.strip_prefix(&session_prefix) else { continue };
        on_file();
        let mut abs = sessions_root.clone();
        for c in rest.split('/') {
            abs.push(c);
        }
        if let Some(st) = state.files.get(logical) {
            // State is trusted only while the file is really there — a
            // synced-then-cleaned file must re-download, not skip forever.
            if st.deleted_locally || (st.hash == meta.hash && abs.exists()) {
                report.unchanged += 1;
                continue;
            }
        }
        if abs.exists() {
            if hash_file(&abs)? == meta.hash {
                report.unchanged += 1;
                continue;
            }
            if mtime_ms(&abs)? > meta.mtime_ms {
                report.skipped_newer_local += 1;
                continue;
            }
        }
        let Some((data, _)) = store.get(logical)? else { continue };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("vibesync-tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &abs)?;
        filetime::set_file_mtime(
            &abs,
            filetime::FileTime::from_unix_time(meta.mtime_ms / 1000, ((meta.mtime_ms % 1000) * 1_000_000) as u32),
        )?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }

    // Write the unioned index back (atomic).
    if !merged.is_empty() {
        let body = merged.values().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n") + "\n";
        let path = index_path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("jsonl.vibesync-tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(report)
}

/// Cheap counts: (session files, bytes, day-dirs, newest mtime).
pub fn light_counts(home: &Path) -> (usize, u64, usize, Option<i64>) {
    let dir = root(home).join("sessions");
    let mut n = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    let mut days = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            n += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(m) = mtime_ms(p) {
                last = Some(last.map_or(m, |l: i64| l.max(m)));
            }
            if let Some(parent) = p.parent() {
                days.insert(parent.to_path_buf());
            }
        }
    }
    (n, bytes, days.len(), last)
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}


// ---------------------------------------------------------------- db layer
//
// Modern Codex (Desktop + CLI, 2026-07) lists threads from
// `~/.codex/state_<N>.sqlite` (generation-named; 40+ sqlx migrations
// observed) — session_index.jsonl is a dead layer kept only for old builds.
// Conversation content still lives in the rollout files, which sync as
// plain files; the db rows are the LISTING, so each thread exports as one
// versioned store object under `codex/db/` and merges with the exact
// recipe proven on OpenCode: newer-wins by max timestamp, never delete,
// one-time backup, generic column maps for schema drift, parking on
// unresolved path tokens, and a rollout gate so a thread only appears once
// its transcript is actually on disk.

pub const DB_PREFIX: &str = "codex/db";

/// Path fields inside a thread row that must translate across machines.
const THREAD_PATH_FIELDS: &[&str] = &["cwd", "rollout_path", "agent_path"];

/// Codex's thread db is generation-named (`state_5.sqlite` today) — pick
/// the highest generation present so a future migration keeps working.
pub fn state_db(home: &Path) -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(root(home)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name
                .strip_prefix("state_")
                .and_then(|r| r.strip_suffix(".sqlite"))
                .and_then(|n| n.parse::<u32>().ok())
            {
                if best.as_ref().map(|(b, _)| n > *b).unwrap_or(true) {
                    best = Some((n, e.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn open_ro(db: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok(conn)
}

/// Directories Codex threads anchor to on this machine — fed to the project
/// map so repos used with Codex tokenize as ${GIT:...}.
pub fn local_dirs(home: &Path) -> Vec<PathBuf> {
    let Some(db) = state_db(home) else { return vec![] };
    let Ok(conn) = open_ro(&db) else { return vec![] };
    let mut out = Vec::new();
    if let Ok(mut st) = conn.prepare("SELECT DISTINCT cwd FROM threads") {
        if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
            out.extend(rows.flatten().map(PathBuf::from).filter(|p| p.is_dir()));
        }
    }
    out
}

/// A thread's export version: the newest of its second- and ms-resolution
/// timestamps (the schema carries both; triggers backfill the ms columns).
fn thread_eff(t: &serde_json::Map<String, serde_json::Value>) -> i64 {
    let ms = ["updated_at_ms", "created_at_ms", "recency_at_ms"]
        .into_iter()
        .filter_map(|k| t.get(k).and_then(|v| v.as_i64()));
    let secs = ["updated_at", "created_at"]
        .into_iter()
        .filter_map(|k| t.get(k).and_then(|v| v.as_i64()).map(|s| s * 1000));
    ms.chain(secs).max().unwrap_or(0)
}

/// sandbox_policy is a JSON string whose writable_roots are machine paths.
fn map_sandbox_paths(
    m: &mut serde_json::Map<String, serde_json::Value>,
    f: &dyn Fn(&str) -> String,
) {
    let Some(serde_json::Value::String(s)) = m.get("sandbox_policy") else { return };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(s) else { return };
    if let Some(roots) = v.get_mut("writable_roots").and_then(|r| r.as_array_mut()) {
        for r in roots.iter_mut() {
            if let Some(p) = r.as_str() {
                *r = serde_json::Value::String(f(p));
            }
        }
    }
    if let Ok(out) = serde_json::to_string(&v) {
        m.insert("sandbox_policy".to_string(), serde_json::Value::String(out));
    }
}

/// Export every thread (+ its dynamic tools and spawn edge) as one store
/// object, diffed against the store LISTING — versions, not bytes, decide.
pub fn db_push(
    home: &Path,
    tok: &crate::tokenizer::Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    let Some(db) = state_db(home) else {
        crate::dlog::debug(|| "codex db: no state_<N>.sqlite found — nothing to export".to_string());
        return Ok(0);
    };
    let conn = open_ro(&db)?;
    let summary: (i64, i64, i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM threads),
                (SELECT COALESCE(MAX(COALESCE(updated_at_ms, updated_at * 1000)), 0) FROM threads),
                (SELECT COUNT(*) FROM thread_dynamic_tools),
                (SELECT COUNT(*) FROM thread_spawn_edges)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    crate::dlog::debug(|| {
        format!("codex db: {} — {} threads, {} dynamic tools", db.display(), summary.0, summary.2)
    });
    let prefix = format!("{DB_PREFIX}/");
    let remote: std::collections::HashMap<&str, (&str, i64)> = listing
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, m)| (k.as_str(), (m.hash.as_str(), m.mtime_ms)))
        .collect();
    let mut store_set: Vec<String> = remote.iter().map(|(k, (h, _))| format!("{k}={h}")).collect();
    store_set.sort();
    let summary_hash = hash_bytes(format!("{summary:?}|{}", store_set.join(";")).as_bytes());
    const SUMMARY_KEY: &str = "codex/db#local-summary";
    if state.files.get(SUMMARY_KEY).map(|s| s.hash == summary_hash).unwrap_or(false) {
        crate::dlog::debug(|| "codex db: db and store unchanged since last export — skipping".to_string());
        return Ok(0);
    }
    let threads = query_maps(&conn, "SELECT * FROM threads", &[])?;
    let mut pushed = 0;
    let mut unchanged = 0;
    for mut t in threads {
        let Some(id) = t.get("id").and_then(|v| v.as_str()).map(String::from) else { continue };
        let tools = query_maps(
            &conn,
            "SELECT * FROM thread_dynamic_tools WHERE thread_id = ?1 ORDER BY position",
            &[&id],
        )?;
        let edges =
            query_maps(&conn, "SELECT * FROM thread_spawn_edges WHERE child_thread_id = ?1", &[&id])?;
        let eff = thread_eff(&t);
        for f in THREAD_PATH_FIELDS {
            tokenize_field(&mut t, f, tok);
        }
        map_sandbox_paths(&mut t, &|p| tok.tokenize_plain(p));
        let obj = serde_json::json!({ "thread": t, "dynamic_tools": tools, "spawn_edges": edges });
        let bytes = serde_json::to_vec(&obj)?;
        let hash = hash_bytes(&bytes);
        let logical = format!("{DB_PREFIX}/{id}.json");
        if let Some((rhash, rmtime)) = remote.get(logical.as_str()) {
            if *rmtime > 0 && (*rhash == hash || *rmtime >= eff) {
                unchanged += 1;
                continue;
            }
        }
        crate::dlog::debug(|| format!("codex db: exporting thread {id} ({} KB)", bytes.len() / 1024));
        store.put(
            &logical,
            &bytes,
            &RemoteMeta { hash: hash.clone(), mtime_ms: eff, size: bytes.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms: eff, size: bytes.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    state.files.insert(
        SUMMARY_KEY.to_string(),
        FileState { hash: summary_hash, mtime_ms: 0, size: 0, deleted_locally: false },
    );
    if pushed > 0 {
        crate::dlog::info(|| format!("codex db: exported {pushed} thread(s), {unchanged} already in store"));
    } else {
        crate::dlog::debug(|| format!("codex db: nothing to export ({unchanged} already in store)"));
    }
    Ok(pushed)
}

/// Merge foreign threads into the local db: insert missing, update only
/// when the remote is newer, never delete; the thread row itself is
/// replaced only when the remote ROW is newer. A thread only applies once
/// its rollout file exists locally (the file layer delivers it in the same
/// sync), so Codex never lists a session it cannot open.
pub fn db_apply(
    home: &Path,
    tok: &crate::tokenizer::Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let Some(db) = state_db(home) else { return Ok(report) };
    let prefix = format!("{DB_PREFIX}/");
    let mut seen = 0usize;
    let mut awaiting = 0usize;
    let mut conn: Option<rusqlite::Connection> = None;
    for (logical, meta) in listing {
        if !logical.starts_with(&prefix) {
            continue;
        }
        seen += 1;
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally
                || st.hash == meta.hash
                || (st.mtime_ms > 0 && st.mtime_ms >= meta.mtime_ms)
            {
                report.unchanged += 1;
                continue;
            }
        }
        let Some((bytes, _)) = store.get(logical)? else { continue };
        let Ok(mut obj) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        let Some(t) = obj.get_mut("thread").and_then(|s| s.as_object_mut()) else { continue };
        for f in THREAD_PATH_FIELDS {
            expand_field(t, f, tok);
        }
        map_sandbox_paths(t, &|p| crate::dbsync::normalize_path_shape(&tok.expand_plain(p)));
        let parked = THREAD_PATH_FIELDS.iter().any(|k| {
            t.get(*k)
                .and_then(|v| v.as_str())
                .map(crate::gitmap::has_unresolved_token)
                .unwrap_or(false)
        });
        if parked {
            crate::dlog::debug(|| format!("codex db: {logical} parked — project not on this machine"));
            report.parked += 1;
            continue;
        }
        let (Some(id), rollout) = (
            t.get("id").and_then(|v| v.as_str()).map(String::from),
            t.get("rollout_path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ) else {
            continue;
        };
        let remote_eff = thread_eff(t);
        let remote_row_t = t
            .get("updated_at_ms")
            .and_then(|v| v.as_i64())
            .or_else(|| t.get("updated_at").and_then(|v| v.as_i64()).map(|s| s * 1000))
            .unwrap_or(0);
        if conn.is_none() {
            let bak = db.with_extension("sqlite.vibesync-bak");
            if !bak.exists() {
                let _ = std::fs::copy(&db, &bak);
            }
            let c = rusqlite::Connection::open(&db)?;
            c.busy_timeout(std::time::Duration::from_millis(1500))?;
            conn = Some(c);
        }
        let c = conn.as_ref().unwrap();
        let local_row = query_maps(c, "SELECT * FROM threads WHERE id = ?1", &[&id])?.pop();
        if let Some(lr) = &local_row {
            if thread_eff(lr) >= remote_eff {
                // Codex backfills threads from synced rollout files BEFORE
                // our merge runs, planting raw foreign cwds the UI can never
                // scope to a workspace here. A kept row still gets its path
                // fields healed from the remote's translated values.
                let mut healed = Vec::new();
                for f in THREAD_PATH_FIELDS {
                    let local_v = lr.get(*f).and_then(|v| v.as_str()).unwrap_or("");
                    let remote_v = t.get(*f).and_then(|v| v.as_str()).unwrap_or("");
                    if !remote_v.is_empty()
                        && crate::dbsync::foreign_shaped(local_v, tok.home())
                        && !crate::dbsync::foreign_shaped(remote_v, tok.home())
                        && !crate::gitmap::has_unresolved_token(remote_v)
                    {
                        healed.push((*f, remote_v.to_string()));
                    }
                }
                for (f, v) in &healed {
                    c.execute(
                        &format!("UPDATE threads SET `{f}` = ?1 WHERE id = ?2"),
                        rusqlite::params![v, id],
                    )?;
                }
                if !healed.is_empty() {
                    crate::dlog::debug(|| {
                        format!("codex db: {id} kept — {} path field(s) healed to local shape", healed.len())
                    });
                } else {
                    crate::dlog::debug(|| format!("codex db: {id} kept — local copy is same or newer"));
                }
                report.skipped_newer_local += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                );
                continue;
            }
        }
        // Rollout gate AFTER the newer-local check, so an already-known
        // thread doesn't re-download its export every sync while waiting.
        if !rollout.is_empty() && !Path::new(&rollout).exists() {
            crate::dlog::debug(|| {
                format!("codex db: {id} awaiting its rollout file ({rollout}) — retrying next sync")
            });
            report.parked += 1;
            awaiting += 1;
            continue;
        }
        crate::dlog::debug(|| format!("codex db: merging thread {id}"));
        let local_row_t = local_row.as_ref().map(thread_eff).unwrap_or(0);
        let t = obj.get("thread").and_then(|s| s.as_object()).unwrap();
        if local_row.is_none() || remote_row_t > local_row_t {
            insert_map(c, "threads", t, true)?;
        }
        for row in obj.get("dynamic_tools").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(row) = row.as_object() {
                insert_map(c, "thread_dynamic_tools", row, true)?;
            }
        }
        for row in obj.get("spawn_edges").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(row) = row.as_object() {
                insert_map(c, "thread_spawn_edges", row, true)?;
            }
        }
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }
    crate::dlog::debug(|| {
        format!(
            "codex db: store holds {seen} thread export(s) — {} merged, {} unchanged, {} kept (local newer), {} parked/awaiting ({awaiting} awaiting rollout)",
            report.applied, report.unchanged, report.skipped_newer_local, report.parked
        )
    });
    Ok(report)
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::store::FolderStore;
    use crate::tokenizer::Tokenizer;

    fn make_state_db(home: &std::path::Path) -> PathBuf {
        let p = home.join(".codex/state_5.sqlite");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
               source TEXT NOT NULL, model_provider TEXT NOT NULL, cwd TEXT NOT NULL,
               title TEXT NOT NULL, sandbox_policy TEXT NOT NULL, approval_mode TEXT NOT NULL,
               created_at_ms INTEGER, updated_at_ms INTEGER, recency_at_ms INTEGER NOT NULL DEFAULT 0,
               git_origin_url TEXT);
             CREATE TABLE thread_dynamic_tools (thread_id TEXT NOT NULL, position INTEGER NOT NULL,
               name TEXT NOT NULL, description TEXT NOT NULL, input_schema TEXT NOT NULL,
               PRIMARY KEY(thread_id, position));
             CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL,
               child_thread_id TEXT NOT NULL PRIMARY KEY, status TEXT NOT NULL);",
        )
        .unwrap();
        p
    }

    #[test]
    fn db_threads_roundtrip_with_rollout_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let db_a = make_state_db(&a);
        let a_home = a.to_string_lossy().into_owned();
        {
            let c = rusqlite::Connection::open(&db_a).unwrap();
            c.execute(
                "INSERT INTO threads VALUES ('th1', ?1, 100, 200, 'vscode', 'openai', ?2,
                   'Test thread', ?3, 'on-request', 100000, 200000, 200000, NULL)",
                rusqlite::params![
                    format!("{a_home}/.codex/sessions/2026/07/12/rollout-th1.jsonl"),
                    format!("{a_home}/proj"),
                    format!("{{\"type\":\"workspace-write\",\"writable_roots\":[\"{a_home}/proj\"]}}"),
                ],
            )
            .unwrap();
            c.execute("INSERT INTO thread_dynamic_tools VALUES ('th1', 0, 'tool_a', 'desc', '{}')", [])
                .unwrap();
        }
        let tok_a = Tokenizer::with_case_sensitivity(&a_home, false);
        let mut st_a = SyncState::default();
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &[]).unwrap(), 1);
        // Same content, listing now has it: no re-push (version rule).
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 0);

        // Machine B: thread must NOT apply until its rollout file exists.
        let b = tmp.path().join("b");
        let db_b = make_state_db(&b);
        let b_home = b.to_string_lossy().into_owned();
        let tok_b = Tokenizer::with_case_sensitivity(&b_home, false);
        let mut st_b = SyncState::default();
        let listing = store.list().unwrap();
        let r = db_apply(&b, &tok_b, &mut st_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!((r.applied, r.parked), (0, 1));

        // Rollout lands (the file layer delivers it) -> thread applies with
        // every machine path translated.
        let ro = b.join(".codex/sessions/2026/07/12/rollout-th1.jsonl");
        std::fs::create_dir_all(ro.parent().unwrap()).unwrap();
        std::fs::write(&ro, "{}").unwrap();
        let r = db_apply(&b, &tok_b, &mut st_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(r.applied, 1);
        let c = rusqlite::Connection::open(&db_b).unwrap();
        let (cwd, rp, sp): (String, String, String) = c
            .query_row("SELECT cwd, rollout_path, sandbox_policy FROM threads WHERE id='th1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(cwd, format!("{b_home}/proj"));
        assert_eq!(rp, ro.to_string_lossy());
        assert!(sp.contains(&format!("{b_home}/proj")), "{sp}");
        let tools: i64 =
            c.query_row("SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id='th1'", [], |r| r.get(0)).unwrap();
        assert_eq!(tools, 1);

        // Ping-pong guard: B's byte-divergent re-export at equal version
        // must not push.
        c.execute("UPDATE threads SET title='renamed locally' WHERE id='th1'", []).unwrap();
        assert_eq!(db_push(&b, &tok_b, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(), 0);

        // Newer local thread: untouched by an older remote.
        c.execute("UPDATE threads SET updated_at_ms=999000, updated_at=999 WHERE id='th1'", []).unwrap();
        let mut st_b2 = SyncState::default();
        let r = db_apply(&b, &tok_b, &mut st_b2, &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        // ...and B's strictly newer copy pushes over the store's.
        assert_eq!(db_push(&b, &tok_b, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn sync_residue_is_not_an_install() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".codex/sessions/2026/01/01")).unwrap();
        std::fs::write(home.join(".codex/session_index.jsonl"), "{}\n").unwrap();
        assert!(!detect(home), "sessions+index alone are sync residue");
        std::fs::write(home.join(".codex/config.toml"), "").unwrap();
        assert!(detect(home), "a real install marker flips detection");
    }

    #[test]
    fn sessions_sync_and_index_unions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));

        // Machine A: one session + index listing it.
        let a = tmp.path().join("a");
        let sess_a = a.join(".codex/sessions/2026/04/21");
        std::fs::create_dir_all(&sess_a).unwrap();
        std::fs::write(sess_a.join("rollout-aaa.jsonl"), "{\"m\":1}\n").unwrap();
        std::fs::write(
            a.join(".codex/session_index.jsonl"),
            "{\"id\":\"aaa\",\"thread_name\":\"A\",\"updated_at\":\"2026-04-21T09:00:00Z\"}\n",
        )
        .unwrap();

        let mut state_a = SyncState::default();
        crate::sync::push(&scan(&a).unwrap(), &mut state_a, &store, "a").unwrap();
        push_index(&a, "machine-a", &mut state_a, &store).unwrap();

        // Machine B: its own session + index; then apply A's.
        let b = tmp.path().join("b");
        let sess_b = b.join(".codex/sessions/2026/05/01");
        std::fs::create_dir_all(&sess_b).unwrap();
        std::fs::write(sess_b.join("rollout-bbb.jsonl"), "{\"m\":2}\n").unwrap();
        std::fs::write(
            b.join(".codex/session_index.jsonl"),
            "{\"id\":\"bbb\",\"thread_name\":\"B\",\"updated_at\":\"2026-05-01T09:00:00Z\"}\n",
        )
        .unwrap();

        let mut state_b = SyncState::default();
        let listing = store.list().unwrap();
        let report = apply(&b, &mut state_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(report.applied, 1); // A's session file landed
        assert!(b.join(".codex/sessions/2026/04/21/rollout-aaa.jsonl").exists());

        // B's index now lists BOTH sessions.
        let idx = std::fs::read_to_string(b.join(".codex/session_index.jsonl")).unwrap();
        assert!(idx.contains("\"aaa\""), "{idx}");
        assert!(idx.contains("\"bbb\""), "{idx}");
    }
}
