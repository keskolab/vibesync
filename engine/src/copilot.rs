//! GitHub Copilot CLI adapter.
//!
//! Storage (validated on macOS, 2026-07): `~/.copilot/` holds
//! - `session-state/<uuid>/` — one dir per session (state + metadata files).
//!   UUID-named, so files sync purely additively across machines.
//! - `logs/`, `restart/`, `ide/` — volatile, never synced.
//! - `config.json`, `settings.json` — local auth/trust settings, never synced.
//! - `vscode.session.*` — caches for the VS Code bridge, never synced.
//!
//! Sessions opened through the VS Code integration keep their chat content in
//! VS Code's own storage (covered by the vscode adapter); this adapter makes
//! standalone `copilot` CLI sessions follow the user. Interior absolute paths
//! inside session state are not rewritten (same policy as Claude transcripts).
//!
//! DB LAYER (validated on Windows, 2026-07-13): current Copilot CLI builds
//! keep the conversation itself in `~/.copilot/session-store.db` (WAL) —
//! `sessions` + `turns`/`checkpoints`/`session_files`/`session_refs` — and
//! the session-state dir holds only workspace.yaml + empty scaffolding, so
//! the file layer alone syncs an EMPTY shell (the exact VS Code panel-index
//! failure mode: dirs arrive, nothing to resume). `db_push`/`db_apply`
//! mirror the Codex thread-db recipe: one store object per session under
//! `copilot/db/`, versioned by the newest ISO timestamp across the session
//! and all child rows, newer-wins/never-delete, two-slot #own state,
//! deletion tombstones, unresolved-token parking, one-time .vibesync-bak.
//! Machine-local AUTOINCREMENT ids are stripped from child rows; they merge
//! on their natural UNIQUE keys instead. The FTS search_index is derived
//! data and never synced (foreign sessions aren't full-text searchable).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scanner::{hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const PREFIX: &str = "copilot/session-state";

fn root(home: &Path) -> PathBuf {
    home.join(".copilot")
}

/// Installed = ~/.copilot holds something a real install writes (config,
/// logs, settings). A dir with only session-state/ is sync residue.
pub fn detect(home: &Path) -> bool {
    let dir = root(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        crate::dlog::debug(|| format!("detect copilot: NOT installed ({} missing)", dir.display()));
        return false;
    };
    let found = rd.flatten().any(|e| e.file_name() != "session-state");
    crate::dlog::debug(|| {
        format!(
            "detect copilot: {} ({})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            dir.display()
        )
    });
    found
}

pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let dir = root(home).join("session-state");
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
        let rel = path
            .strip_prefix(&dir)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(FileEntry {
            logical: format!("{PREFIX}/{rel}"),
            abs: path.to_path_buf(),
            size: entry.metadata()?.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let dir = root(home).join("session-state");
    let prefix = format!("{PREFIX}/");
    for (logical, meta) in listing {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        on_file();
        let mut abs = dir.clone();
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
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }
    Ok(report)
}

// ---------------------------------------------------------------- db layer

pub const DB_PREFIX: &str = "copilot/db";
const SUMMARY_KEY: &str = "copilot/db#local-summary";

/// (table, natural conflict key, timestamp column) for every child table
/// that travels with a session. AUTOINCREMENT ids are machine-local and
/// stripped; rows merge on the natural key.
const CHILDREN: &[(&str, &[&str], &str)] = &[
    ("turns", &["session_id", "turn_index"], "timestamp"),
    ("checkpoints", &["session_id", "checkpoint_number"], "created_at"),
    ("session_files", &["session_id", "file_path"], "first_seen_at"),
    ("session_refs", &["session_id", "ref_type", "ref_value"], "created_at"),
];

pub fn store_db(home: &Path) -> PathBuf {
    root(home).join("session-store.db")
}

fn open_ro(db: &Path) -> Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    Ok(conn)
}

/// Copilot timestamps are ISO-8601 TEXT (`2026-07-13T12:41:13.292Z`, or
/// SQLite's `2026-07-13 12:41:13` default form) — parse to epoch ms so
/// they can version store objects. Unparseable → 0 (never blocks a merge).
fn iso_ms(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 19 {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(sec)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return 0;
    };
    let ms = if b.len() >= 23 && b[19] == b'.' { num(20..23).unwrap_or(0) } else { 0 };
    // Days-from-civil (Howard Hinnant's algorithm).
    let y2 = y - if mo <= 2 { 1 } else { 0 };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (((days * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms
}

fn map_iso(m: &serde_json::Map<String, serde_json::Value>, key: &str) -> i64 {
    m.get(key).and_then(|v| v.as_str()).map(iso_ms).unwrap_or(0)
}

/// A session export's version: the newest timestamp across the session row
/// and every child row (a late turn must travel even when the session row's
/// updated_at didn't move).
fn export_eff(obj: &serde_json::Value) -> i64 {
    let mut eff = 0i64;
    if let Some(s) = obj.get("session").and_then(|v| v.as_object()) {
        eff = eff.max(map_iso(s, "created_at")).max(map_iso(s, "updated_at"));
    }
    for (table, _, ts) in CHILDREN {
        for row in obj.get(*table).and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(r) = row.as_object() {
                eff = eff.max(map_iso(r, ts));
            }
        }
    }
    eff
}

/// Directories Copilot sessions anchor to on this machine — fed to the
/// project map so repos used with Copilot CLI tokenize as ${GIT:...}.
pub fn local_dirs(home: &Path) -> Vec<PathBuf> {
    let db = store_db(home);
    if !db.exists() {
        return vec![];
    }
    let Ok(conn) = open_ro(&db) else { return vec![] };
    let mut out = Vec::new();
    if let Ok(mut st) = conn.prepare("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL") {
        if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
            out.extend(rows.flatten().map(PathBuf::from).filter(|p| p.is_dir()));
        }
    }
    out
}

/// Export every session (+ child rows) as one store object, diffed against
/// the store LISTING — versions, not bytes, decide (same recipe as Codex).
pub fn db_push(
    home: &Path,
    tok: &crate::tokenizer::Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    let db = store_db(home);
    if !db.exists() {
        crate::dlog::debug(|| "copilot db: no session-store.db — nothing to export".to_string());
        return Ok(0);
    }
    let conn = open_ro(&db)?;
    let summary: (i64, String, i64, String, String) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sessions),
                (SELECT COALESCE(MAX(updated_at), '') FROM sessions),
                (SELECT COUNT(*) FROM turns),
                (SELECT COALESCE(MAX(timestamp), '') FROM turns),
                (SELECT COALESCE(group_concat(id || COALESCE(cwd, '')), '')
                   FROM (SELECT id, cwd FROM sessions ORDER BY id))",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    crate::dlog::debug(|| {
        format!("copilot db: {} — {} sessions, {} turns", db.display(), summary.0, summary.2)
    });
    let prefix = format!("{DB_PREFIX}/");
    let remote: std::collections::HashMap<&str, (&str, i64)> = listing
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, m)| (k.as_str(), (m.hash.as_str(), m.mtime_ms)))
        .collect();
    let mut store_set: Vec<String> = remote.iter().map(|(k, (h, _))| format!("{k}={h}")).collect();
    store_set.sort();
    let summary_hash =
        crate::scanner::hash_bytes(format!("{summary:?}|{}", store_set.join(";")).as_bytes());
    if state.files.get(SUMMARY_KEY).map(|s| s.hash == summary_hash).unwrap_or(false) {
        crate::dlog::debug(|| "copilot db: db and store unchanged since last export — skipping".to_string());
        return Ok(0);
    }
    let sessions = crate::dbsync::query_maps(&conn, "SELECT * FROM sessions", &[])?;
    // Deletion tombstones: a session this machine previously merged or
    // exported that is GONE from the local db was deleted by Copilot —
    // without a tombstone the next apply re-inserts it forever.
    {
        let live: std::collections::HashSet<String> = sessions
            .iter()
            .filter_map(|s| s.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let mut dead: Vec<String> = Vec::new();
        for (k, st) in state.files.iter() {
            if st.deleted_locally || !k.starts_with(&prefix) || k.ends_with("#own") || k == SUMMARY_KEY {
                continue;
            }
            let Some(id) = k.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".json")) else {
                continue;
            };
            if !live.contains(id) {
                dead.push(k.clone());
            }
        }
        for k in dead {
            crate::dlog::debug(|| format!("copilot db: {k} deleted locally — tombstoned, will not resurrect here"));
            if let Some(st) = state.files.get_mut(&k) {
                st.deleted_locally = true;
            }
            state.files.remove(&format!("{k}#own"));
        }
    }
    let mut pushed = 0;
    let mut unchanged = 0;
    for mut s in sessions {
        let Some(id) = s.get("id").and_then(|v| v.as_str()).map(String::from) else { continue };
        let mut obj = serde_json::Map::new();
        for (table, keys, _) in CHILDREN {
            let order = keys.join("`,`");
            let mut rows = crate::dbsync::query_maps(
                &conn,
                &format!("SELECT * FROM `{table}` WHERE session_id = ?1 ORDER BY `{order}`"),
                &[&id],
            )?;
            for r in rows.iter_mut() {
                r.remove("id"); // AUTOINCREMENT — meaningless off-machine
                if *table == "session_files" {
                    crate::dbsync::tokenize_field(r, "file_path", tok);
                }
            }
            obj.insert(table.to_string(), serde_json::Value::Array(
                rows.into_iter().map(serde_json::Value::Object).collect(),
            ));
        }
        crate::dbsync::tokenize_field(&mut s, "cwd", tok);
        obj.insert("session".to_string(), serde_json::Value::Object(s));
        let obj = serde_json::Value::Object(obj);
        let eff = export_eff(&obj);
        let bytes = serde_json::to_vec(&obj)?;
        let hash = crate::scanner::hash_bytes(&bytes);
        let logical = format!("{DB_PREFIX}/{id}.json");
        // Two-slot state, same reasoning as Codex: main slot = store version
        // SEEN (apply writes it), #own slot = our canonical bytes; publish
        // consults #own so re-serialization drift can't ping-pong.
        let own_slot = format!("{logical}#own");
        let own_prev = state.files.get(&own_slot).map(|s| s.hash == hash).unwrap_or(false)
            || state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false);
        state.files.insert(
            own_slot,
            FileState { hash: hash.clone(), mtime_ms: eff, size: 0, deleted_locally: false },
        );
        if let Some((rhash, rmtime)) = remote.get(logical.as_str()) {
            if *rmtime > 0 && (*rhash == hash || *rmtime > eff || (*rmtime >= eff && own_prev)) {
                unchanged += 1;
                continue;
            }
        }
        crate::dlog::debug(|| format!("copilot db: exporting session {id} ({} KB)", bytes.len() / 1024));
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
        crate::dlog::info(|| format!("copilot db: exported {pushed} session(s), {unchanged} already in store"));
    } else {
        crate::dlog::debug(|| format!("copilot db: nothing to export ({unchanged} already in store)"));
    }
    Ok(pushed)
}

/// Merge foreign sessions into the local db: insert missing, replace the
/// session row only when the remote ROW is newer, child rows merge on their
/// natural keys, never delete. A session applies only once its session-state
/// dir exists locally (the file layer delivers it in the same sync), so
/// Copilot never lists a session missing its on-disk scaffolding.
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
    let db = store_db(home);
    if !db.exists() {
        crate::dlog::debug(|| "copilot db: no session-store.db yet — merge waits for first local run".to_string());
        return Ok(report);
    }
    let prefix = format!("{DB_PREFIX}/");
    let mut awaiting = 0usize;
    let mut conn: Option<rusqlite::Connection> = None;
    // Adopt foreign-home cwds and heal mixed separators independent of the
    // store loop (state-recorded objects are never refetched, so a
    // loop-dependent heal would starve — same lesson as Codex).
    {
        let ro = open_ro(&db)?;
        let rows = crate::dbsync::query_maps(&ro, "SELECT id, cwd FROM sessions WHERE cwd IS NOT NULL", &[])?;
        drop(ro);
        let mut fixes: Vec<(String, String)> = Vec::new();
        for r in &rows {
            let (Some(id), Some(cwd)) = (
                r.get("id").and_then(|v| v.as_str()),
                r.get("cwd").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if let Some(adopted) = crate::dbsync::adopt_foreign_home(cwd, tok.home()) {
                fixes.push((id.to_string(), adopted));
            } else {
                let n = crate::dbsync::normalize_path_shape(cwd);
                if n != cwd {
                    fixes.push((id.to_string(), n));
                }
            }
        }
        if !fixes.is_empty() {
            let bak = db.with_extension("db.vibesync-bak");
            if !bak.exists() {
                let _ = std::fs::copy(&db, &bak);
            }
            let c = rusqlite::Connection::open(&db)?;
            c.busy_timeout(std::time::Duration::from_millis(1500))?;
            for (id, v) in &fixes {
                c.execute("UPDATE sessions SET cwd = ?1 WHERE id = ?2", rusqlite::params![v, id])?;
            }
            crate::dlog::info(|| {
                format!("copilot db: adopted {} foreign cwd(s) onto this machine's home", fixes.len())
            });
            conn = Some(c);
        }
    }
    for (logical, meta) in listing {
        if !logical.starts_with(&prefix) {
            continue;
        }
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally
                || st.hash == meta.hash
                || (st.mtime_ms > 0 && st.mtime_ms > meta.mtime_ms)
            {
                report.unchanged += 1;
                continue;
            }
        }
        let Some((bytes, _)) = store.get(logical)? else { continue };
        let Ok(mut obj) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        let Some(s) = obj.get_mut("session").and_then(|v| v.as_object_mut()) else { continue };
        crate::dbsync::expand_field(s, "cwd", tok);
        if s.get("cwd")
            .and_then(|v| v.as_str())
            .map(crate::gitmap::has_unresolved_token)
            .unwrap_or(false)
        {
            crate::dlog::debug(|| format!("copilot db: {logical} parked — project not on this machine"));
            report.parked += 1;
            continue;
        }
        let Some(id) = s.get("id").and_then(|v| v.as_str()).map(String::from) else { continue };
        let remote_eff = export_eff(&obj);
        let remote_row_t = {
            let s = obj.get("session").and_then(|v| v.as_object()).unwrap();
            map_iso(s, "updated_at").max(map_iso(s, "created_at"))
        };
        if conn.is_none() {
            let bak = db.with_extension("db.vibesync-bak");
            if !bak.exists() {
                let _ = std::fs::copy(&db, &bak);
            }
            let c = rusqlite::Connection::open(&db)?;
            c.busy_timeout(std::time::Duration::from_millis(1500))?;
            conn = Some(c);
        }
        let c = conn.as_ref().unwrap();
        let local_row =
            crate::dbsync::query_maps(c, "SELECT * FROM sessions WHERE id = ?1", &[&id])?.pop();
        if let Some(lr) = &local_row {
            let mut local_eff = map_iso(lr, "updated_at").max(map_iso(lr, "created_at"));
            for (table, _, ts) in CHILDREN {
                let m: String = c
                    .query_row(
                        &format!("SELECT COALESCE(MAX(`{ts}`), '') FROM `{table}` WHERE session_id = ?1"),
                        [&id],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                local_eff = local_eff.max(iso_ms(&m));
            }
            if local_eff >= remote_eff {
                crate::dlog::debug(|| format!("copilot db: {id} kept — local copy is same or newer"));
                report.skipped_newer_local += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                );
                continue;
            }
        }
        // Scaffolding gate AFTER the newer-local check, so a known session
        // doesn't re-download its export every sync while waiting.
        let state_dir = root(home).join("session-state").join(&id);
        if !state_dir.exists() {
            crate::dlog::debug(|| {
                format!("copilot db: {id} awaiting its session-state dir — retrying next sync")
            });
            report.parked += 1;
            awaiting += 1;
            continue;
        }
        crate::dlog::debug(|| format!("copilot db: merging session {id}"));
        let local_row_t = local_row
            .as_ref()
            .map(|lr| map_iso(lr, "updated_at").max(map_iso(lr, "created_at")))
            .unwrap_or(0);
        let s = obj.get("session").and_then(|v| v.as_object()).unwrap();
        if local_row.is_none() || remote_row_t > local_row_t {
            crate::dbsync::insert_map_pk(c, "sessions", s, true, &["id"])?;
        }
        for (table, keys, _) in CHILDREN {
            for row in obj.get(*table).and_then(|v| v.as_array()).into_iter().flatten() {
                let Some(row) = row.as_object() else { continue };
                let mut row = row.clone();
                row.remove("id"); // defensive: old exports may carry it
                if *table == "session_files" {
                    crate::dbsync::expand_field(&mut row, "file_path", tok);
                    if row
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .map(crate::gitmap::has_unresolved_token)
                        .unwrap_or(false)
                    {
                        continue; // auxiliary row for a project not here
                    }
                }
                crate::dbsync::insert_map_pk(c, table, &row, true, keys)?;
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
            "copilot db: {} merged, {} unchanged, {} kept (local newer), {} parked ({awaiting} awaiting session-state dir)",
            report.applied, report.unchanged, report.skipped_newer_local, report.parked
        )
    });
    Ok(report)
}

/// (sessions, bytes, newest activity ms).
pub fn light_counts(home: &Path) -> (usize, u64, Option<i64>) {
    let dir = root(home).join("session-state");
    let mut sessions = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            sessions += 1;
            for f in walkdir::WalkDir::new(e.path()).into_iter().flatten() {
                if f.file_type().is_file() {
                    bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Ok(m) = mtime_ms(f.path()) {
                        last = Some(last.map_or(m, |l: i64| l.max(m)));
                    }
                }
            }
        }
    }
    (sessions, bytes, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn sync_residue_is_not_an_install() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".copilot/session-state/abc")).unwrap();
        assert!(!detect(home), "session-state alone is sync residue");
        std::fs::write(home.join(".copilot/config.json"), "{}").unwrap();
        assert!(detect(home), "a real install marker flips detection");
    }

    /// Replica of the real session-store.db schema (Windows, 2026-07-13).
    fn make_store_db(home: &Path) -> PathBuf {
        let p = home.join(".copilot/session-store.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT,
               host_type TEXT, branch TEXT, summary TEXT,
               created_at TEXT DEFAULT (datetime('now')),
               updated_at TEXT DEFAULT (datetime('now')));
             CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL REFERENCES sessions(id),
               turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT,
               timestamp TEXT DEFAULT (datetime('now')), UNIQUE(session_id, turn_index));
             CREATE TABLE checkpoints (id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL REFERENCES sessions(id),
               checkpoint_number INTEGER NOT NULL, title TEXT, overview TEXT,
               history TEXT, work_done TEXT, technical_details TEXT,
               important_files TEXT, next_steps TEXT,
               created_at TEXT DEFAULT (datetime('now')),
               UNIQUE(session_id, checkpoint_number));
             CREATE TABLE session_files (id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL REFERENCES sessions(id),
               file_path TEXT NOT NULL, tool_name TEXT, turn_index INTEGER,
               first_seen_at TEXT DEFAULT (datetime('now')),
               UNIQUE(session_id, file_path));
             CREATE TABLE session_refs (id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL REFERENCES sessions(id),
               ref_type TEXT NOT NULL, ref_value TEXT NOT NULL, turn_index INTEGER,
               created_at TEXT DEFAULT (datetime('now')),
               UNIQUE(session_id, ref_type, ref_value));",
        )
        .unwrap();
        p
    }

    fn tok_for(home: &Path) -> crate::tokenizer::Tokenizer {
        crate::tokenizer::Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows))
    }

    #[test]
    fn iso_ms_parses_copilot_timestamps() {
        // 2026-07-13T00:00:00Z = 20647 days * 86400 s.
        assert_eq!(iso_ms("2026-07-13T00:00:00.000Z"), 1_783_900_800_000);
        assert_eq!(iso_ms("2026-07-13T12:41:13.292Z"), 1_783_900_800_000 + ((12 * 60 + 41) * 60 + 13) * 1000 + 292);
        // SQLite's datetime('now') default form.
        assert_eq!(iso_ms("2026-07-13 00:00:01"), 1_783_900_800_000 + 1000);
        assert_eq!(iso_ms("not a date"), 0);
        assert!(iso_ms("2026-07-13T12:41:15.299Z") > iso_ms("2026-07-13T12:41:13.292Z"));
    }

    #[test]
    fn db_round_trip_translates_cwd_and_merges_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let a_home = a.to_string_lossy().into_owned();
        let db_a = make_store_db(&a);
        {
            let c = rusqlite::Connection::open(&db_a).unwrap();
            c.execute(
                "INSERT INTO sessions (id, cwd, repository, summary, created_at, updated_at)
                 VALUES ('s1', ?1, 'o/r', 'Test from A',
                         '2026-07-13T10:00:00.000Z', '2026-07-13T10:05:00.000Z')",
                [format!("{a_home}/proj")],
            )
            .unwrap();
            for (i, msg) in ["hello", "again"].iter().enumerate() {
                c.execute(
                    "INSERT INTO turns (session_id, turn_index, user_message, timestamp)
                     VALUES ('s1', ?1, ?2, '2026-07-13T10:01:00.000Z')",
                    rusqlite::params![i as i64, msg],
                )
                .unwrap();
            }
            c.execute(
                "INSERT INTO session_files (session_id, file_path, first_seen_at)
                 VALUES ('s1', ?1, '2026-07-13T10:02:00.000Z')",
                [format!("{a_home}/proj/main.rs")],
            )
            .unwrap();
        }
        let mut st_a = SyncState::default();
        assert_eq!(db_push(&a, &tok_for(&a), &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 1);
        // Store form is tokenized and carries no machine-local row ids.
        let (bytes, meta) = store.get("copilot/db/s1.json").unwrap().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("${HOME}/proj"), "{text}");
        assert!(!serde_json::from_str::<serde_json::Value>(&text).unwrap()["turns"][0]
            .as_object()
            .unwrap()
            .contains_key("id"));
        assert_eq!(meta.mtime_ms, iso_ms("2026-07-13T10:05:00.000Z"));

        // Machine B: fresh db, session-state dir delivered by the file layer.
        let b = tmp.path().join("b");
        make_store_db(&b);
        std::fs::create_dir_all(b.join(".copilot/session-state/s1")).unwrap();
        let mut st_b = SyncState::default();
        let r = db_apply(&b, &tok_for(&b), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let c = rusqlite::Connection::open(store_db(&b)).unwrap();
        let cwd: String = c
            .query_row("SELECT cwd FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            Path::new(&cwd),
            b.join("proj").as_path(),
            "cwd must land as B's local path"
        );
        let turns: i64 = c.query_row("SELECT COUNT(*) FROM turns WHERE session_id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(turns, 2);
        let fp: String = c
            .query_row("SELECT file_path FROM session_files WHERE session_id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(Path::new(&fp), b.join("proj").join("main.rs").as_path());

        // Re-apply: nothing changes, no duplicate turns.
        let r = db_apply(&b, &tok_for(&b), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 0);
        let turns: i64 = c.query_row("SELECT COUNT(*) FROM turns WHERE session_id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!(turns, 2);
    }

    #[test]
    fn db_newer_local_session_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        make_store_db(&a);
        {
            let c = rusqlite::Connection::open(store_db(&a)).unwrap();
            c.execute(
                "INSERT INTO sessions (id, summary, created_at, updated_at)
                 VALUES ('s2', 'old remote', '2026-07-13T09:00:00.000Z', '2026-07-13T09:00:00.000Z')",
                [],
            )
            .unwrap();
        }
        let mut st_a = SyncState::default();
        db_push(&a, &tok_for(&a), &mut st_a, &store, "a", &store.list().unwrap()).unwrap();

        let b = tmp.path().join("b");
        make_store_db(&b);
        std::fs::create_dir_all(b.join(".copilot/session-state/s2")).unwrap();
        {
            let c = rusqlite::Connection::open(store_db(&b)).unwrap();
            c.execute(
                "INSERT INTO sessions (id, summary, created_at, updated_at)
                 VALUES ('s2', 'newer local', '2026-07-13T09:00:00.000Z', '2026-07-13T11:00:00.000Z')",
                [],
            )
            .unwrap();
        }
        let mut st_b = SyncState::default();
        let r = db_apply(&b, &tok_for(&b), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        let c = rusqlite::Connection::open(store_db(&b)).unwrap();
        let s: String = c.query_row("SELECT summary FROM sessions WHERE id='s2'", [], |r| r.get(0)).unwrap();
        assert_eq!(s, "newer local");
    }

    #[test]
    fn db_session_awaits_its_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        make_store_db(&a);
        {
            let c = rusqlite::Connection::open(store_db(&a)).unwrap();
            c.execute("INSERT INTO sessions (id, summary) VALUES ('s3', 'gated')", []).unwrap();
        }
        let mut st_a = SyncState::default();
        db_push(&a, &tok_for(&a), &mut st_a, &store, "a", &store.list().unwrap()).unwrap();

        let b = tmp.path().join("b");
        make_store_db(&b);
        let mut st_b = SyncState::default();
        let r = db_apply(&b, &tok_for(&b), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.parked, 1, "no session-state dir yet: parked, retried next sync");
        // The file layer delivers the dir; the next sync merges.
        std::fs::create_dir_all(b.join(".copilot/session-state/s3")).unwrap();
        let r = db_apply(&b, &tok_for(&b), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
    }

    #[test]
    fn db_push_skips_when_nothing_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        make_store_db(&a);
        {
            let c = rusqlite::Connection::open(store_db(&a)).unwrap();
            c.execute("INSERT INTO sessions (id, summary) VALUES ('s4', 'once')", []).unwrap();
        }
        let mut st = SyncState::default();
        assert_eq!(db_push(&a, &tok_for(&a), &mut st, &store, "a", &store.list().unwrap()).unwrap(), 1);
        assert_eq!(
            db_push(&a, &tok_for(&a), &mut st, &store, "a", &store.list().unwrap()).unwrap(),
            0,
            "second export with unchanged db must be a no-op"
        );
    }

    #[test]
    fn sessions_sync_additively_and_skip_volatile() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let s = a.join(".copilot/session-state/5ebe-uuid");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("state.json"), "{\"title\":\"hello\"}").unwrap();
        // Volatile/local files that must never sync.
        std::fs::create_dir_all(a.join(".copilot/logs")).unwrap();
        std::fs::write(a.join(".copilot/logs/x.log"), "x").unwrap();
        std::fs::write(a.join(".copilot/config.json"), "{}").unwrap();

        let entries = scan(&a).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].logical.ends_with("5ebe-uuid/state.json"));
        let mut state_a = SyncState::default();
        crate::sync::push(&entries, &mut state_a, &store, "a").unwrap();

        let b = tmp.path().join("b");
        std::fs::create_dir_all(b.join(".copilot")).unwrap();
        let mut state_b = SyncState::default();
        let listing = store.list().unwrap();
        let report = apply(&b, &mut state_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(report.applied, 1);
        assert!(b.join(".copilot/session-state/5ebe-uuid/state.json").exists());
    }
}
