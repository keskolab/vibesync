//! OpenCode adapter (file layer).
//!
//! Storage (validated on macOS, 2026-07): OpenCode keeps per-record JSON
//! under `~/.local/share/opencode/storage/{session,message,part,...}/` AND a
//! relational SQLite db (`opencode.db`). Which is authoritative in the
//! current build is unconfirmed, so this adapter syncs ONLY the storage/
//! files (opaque, additive — records are id-keyed and never collide) and
//! never writes the db. That guarantees a safe cross-machine archive.
//!
//! Two layers, both synced:
//! - storage/ per-record JSON files (legacy layer, additive, opaque).
//! - opencode.db rows (CURRENT layer — modern builds write sessions only
//!   here; live-verified 2026-07-12 when a Windows-created session never
//!   appeared in storage/). Each session exports as ONE store object
//!   (session + project + messages + parts) under `opencode/db/`; apply
//!   inserts missing sessions and updates only when the remote is newer
//!   (time_updated), never deletes, and backs the db up once before its
//!   first-ever write. Rows serialize as generic column maps so schema
//!   drift between OpenCode versions degrades gracefully (unknown columns
//!   are dropped on insert, missing ones hit table defaults).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scanner::{hash_bytes, hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};
use crate::tokenizer::Tokenizer;

pub const PREFIX: &str = "opencode/storage";
/// Volatile/derived subdirs we never sync.
const SKIP: &[&str] = &["session_diff", "session_share", "migration"];

/// OpenCode's data root (`~/.local/share/opencode` everywhere in practice;
/// platform data dirs checked as fallback).
fn data_root(home: &Path) -> Option<PathBuf> {
    let mut cands = vec![home.join(".local/share/opencode")];
    for d in [dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten() {
        cands.push(d.join("opencode"));
    }
    cands.into_iter().find(|c| c.is_dir())
}

fn storage_root(home: &Path) -> Option<PathBuf> {
    data_root(home).map(|r| r.join("storage")).filter(|s| s.is_dir())
}

/// Installed = the data root holds something a real install writes (db,
/// auth, bin, logs). Pre-gating VibeSync versions created storage/ on
/// machines without OpenCode — such residue must not read as an install.
pub fn detect(home: &Path) -> bool {
    let Some(root) = data_root(home) else { return false };
    let Ok(rd) = std::fs::read_dir(&root) else { return false };
    rd.flatten().any(|e| e.file_name() != "storage")
}

pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let Some(root) = storage_root(home) else { return Ok(vec![]) };
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|e| {
        !(e.file_type().is_dir()
            && e.file_name().to_str().map(|n| SKIP.contains(&n)).unwrap_or(false))
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
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

#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub pulled: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    // Apply even if the dir doesn't exist yet (first pull creates it).
    let root = home.join(".local/share/opencode/storage");
    let prefix = format!("{PREFIX}/");
    for (logical, meta) in listing {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let mut abs = root.clone();
        for c in rest.split('/') {
            abs.push(c);
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
        report.pulled += 1;
    }
    Ok(report)
}

pub const DB_PREFIX: &str = "opencode/db";

fn db_path(home: &Path) -> Option<PathBuf> {
    data_root(home).map(|r| r.join("opencode.db")).filter(|p| p.exists())
}

/// One row as column-name -> JSON value (NULL/INTEGER/REAL/TEXT only —
/// OpenCode stores blobs as TEXT JSON).
fn row_to_map(row: &rusqlite::Row, cols: &[String]) -> serde_json::Map<String, serde_json::Value> {
    use rusqlite::types::ValueRef;
    let mut m = serde_json::Map::new();
    for (i, c) in cols.iter().enumerate() {
        let v = match row.get_ref(i) {
            Ok(ValueRef::Null) | Err(_) => serde_json::Value::Null,
            Ok(ValueRef::Integer(n)) => serde_json::Value::from(n),
            Ok(ValueRef::Real(f)) => serde_json::Value::from(f),
            Ok(ValueRef::Text(t)) => serde_json::Value::from(String::from_utf8_lossy(t).into_owned()),
            Ok(ValueRef::Blob(b)) => serde_json::Value::from(crate::scanner::hex(b)),
        };
        m.insert(c.clone(), v);
    }
    m
}

fn query_maps(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_map(row, &cols));
    }
    Ok(out)
}

/// Columns of a local table — inserts are filtered to these so an object
/// from a newer OpenCode version can't fail the whole apply.
fn table_cols(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn insert_map(
    conn: &rusqlite::Connection,
    table: &str,
    map: &serde_json::Map<String, serde_json::Value>,
    or_replace: bool,
) -> Result<()> {
    let local: Vec<String> = table_cols(conn, table)?;
    let cols: Vec<&String> = local.iter().filter(|c| map.contains_key(*c)).collect();
    if cols.is_empty() {
        return Ok(());
    }
    let placeholders = (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let names = cols.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(",");
    let verb = if or_replace { "INSERT OR REPLACE" } else { "INSERT OR IGNORE" };
    let sql = format!("{verb} INTO `{table}` ({names}) VALUES ({placeholders})");
    let params: Vec<Box<dyn rusqlite::ToSql>> = cols
        .iter()
        .map(|c| -> Box<dyn rusqlite::ToSql> {
            match &map[*c] {
                serde_json::Value::Null => Box::new(rusqlite::types::Null),
                serde_json::Value::Bool(b) => Box::new(*b as i64),
                serde_json::Value::Number(n) if n.is_i64() => Box::new(n.as_i64().unwrap()),
                serde_json::Value::Number(n) => Box::new(n.as_f64().unwrap_or(0.0)),
                serde_json::Value::String(s) => Box::new(s.clone()),
                other => Box::new(other.to_string()),
            }
        })
        .collect();
    conn.execute(&sql, rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())))?;
    Ok(())
}

fn tokenize_field(m: &mut serde_json::Map<String, serde_json::Value>, key: &str, tok: &Tokenizer) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let t = tok.tokenize_plain(s);
        m.insert(key.to_string(), serde_json::Value::String(t));
    }
}

fn expand_field(m: &mut serde_json::Map<String, serde_json::Value>, key: &str, tok: &Tokenizer) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let e = tok.expand_plain(s);
        m.insert(key.to_string(), serde_json::Value::String(e));
    }
}

/// Export every db session as one store object. Content-hash diffing via
/// state, like the generic pusher.
pub fn db_push(
    home: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
) -> Result<usize> {
    let Some(db) = db_path(home) else { return Ok(0) };
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    // Short-circuit: exporting every session (plus messages/parts) each sync
    // is wasted work when the db hasn't changed. One summary row decides.
    let summary: (i64, i64, i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM session),
                (SELECT COALESCE(MAX(time_updated),0) FROM session),
                (SELECT COUNT(*) FROM message),
                (SELECT COUNT(*) FROM part)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let summary_hash = hash_bytes(format!("{summary:?}").as_bytes());
    const SUMMARY_KEY: &str = "opencode/db#local-summary";
    if state.files.get(SUMMARY_KEY).map(|s| s.hash == summary_hash).unwrap_or(false) {
        return Ok(0);
    }
    let sessions = query_maps(&conn, "SELECT * FROM session", &[])?;
    let mut pushed = 0;
    for mut session in sessions {
        let Some(id) = session.get("id").and_then(|v| v.as_str()).map(String::from) else {
            continue;
        };
        let project = session
            .get("project_id")
            .and_then(|v| v.as_str())
            .map(|pid| query_maps(&conn, "SELECT * FROM project WHERE id = ?1", &[&pid]))
            .transpose()?
            .and_then(|mut v| v.pop());
        let messages =
            query_maps(&conn, "SELECT * FROM message WHERE session_id = ?1 ORDER BY id", &[&id])?;
        let parts =
            query_maps(&conn, "SELECT * FROM part WHERE session_id = ?1 ORDER BY id", &[&id])?;
        tokenize_field(&mut session, "directory", tok);
        let project = project.map(|mut p| {
            tokenize_field(&mut p, "worktree", tok);
            p
        });
        let obj = serde_json::json!({
            "session": session, "project": project, "messages": messages, "parts": parts,
        });
        let bytes = serde_json::to_vec(&obj)?;
        let hash = hash_bytes(&bytes);
        let logical = format!("{DB_PREFIX}/{id}.json");
        if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
            continue;
        }
        store.put(
            &logical,
            &bytes,
            &RemoteMeta { hash: hash.clone(), mtime_ms: 0, size: bytes.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms: 0, size: bytes.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    state.files.insert(
        SUMMARY_KEY.to_string(),
        FileState { hash: summary_hash, mtime_ms: 0, size: 0, deleted_locally: false },
    );
    Ok(pushed)
}

#[derive(Debug, Default, PartialEq)]
pub struct DbApplyReport {
    pub applied: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
    pub parked: usize,
}

/// Merge foreign sessions into opencode.db: insert missing, update only when
/// the remote `time_updated` is newer, never delete. The db is backed up
/// once (`opencode.db.vibesync-bak`) before this build's first-ever write.
pub fn db_apply(
    home: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<DbApplyReport> {
    let mut report = DbApplyReport::default();
    let Some(db) = db_path(home) else { return Ok(report) };
    let prefix = format!("{DB_PREFIX}/");
    let mut conn: Option<rusqlite::Connection> = None;
    for (logical, meta) in listing {
        if !logical.starts_with(&prefix) {
            continue;
        }
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let Some((bytes, _)) = store.get(logical)? else { continue };
        let Ok(mut obj) = serde_json::from_slice::<serde_json::Value>(&bytes) else { continue };
        let Some(session) = obj.get_mut("session").and_then(|s| s.as_object_mut()) else {
            continue;
        };
        expand_field(session, "directory", tok);
        if session
            .get("directory")
            .and_then(|v| v.as_str())
            .map(crate::gitmap::has_unresolved_token)
            .unwrap_or(false)
        {
            report.parked += 1; // project unknown here; retry when it appears
            continue;
        }
        let (Some(id), Some(remote_updated)) = (
            session.get("id").and_then(|v| v.as_str()).map(String::from),
            session.get("time_updated").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        // Open lazily + back up before the first write of this build's life.
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
        let local_updated: Option<i64> = c
            .query_row("SELECT time_updated FROM session WHERE id = ?1", [&id], |r| r.get(0))
            .ok();
        if let Some(local) = local_updated {
            if local >= remote_updated {
                report.skipped_newer_local += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: 0, size: meta.size, deleted_locally: false },
                );
                continue;
            }
        }
        if let Some(project) = obj.get_mut("project").and_then(|p| p.as_object_mut()) {
            expand_field(project, "worktree", tok);
            insert_map(c, "project", project, false)?; // never clobber a local project row
        }
        let session = obj.get("session").and_then(|s| s.as_object()).unwrap();
        insert_map(c, "session", session, true)?;
        for m in obj.get("messages").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(m) = m.as_object() {
                insert_map(c, "message", m, true)?;
            }
        }
        for p in obj.get("parts").and_then(|v| v.as_array()).into_iter().flatten() {
            if let Some(p) = p.as_object() {
                insert_map(c, "part", p, true)?;
            }
        }
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: 0, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }
    Ok(report)
}

/// (sessions, bytes, projects, newest activity ms). Prefers opencode.db
/// (read-only — the authoritative layer on current builds); falls back to
/// counting storage/ files for old installs without the db.
pub fn light_counts(home: &Path) -> (usize, u64, usize, Option<i64>) {
    if let Some(root) = data_root(home) {
        let db = root.join("opencode.db");
        if db.exists() {
            if let Ok(conn) = rusqlite::Connection::open_with_flags(
                &db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            ) {
                let n: usize =
                    conn.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0)).unwrap_or(0);
                let p: usize =
                    conn.query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0)).unwrap_or(0);
                let last: Option<i64> = conn
                    .query_row("SELECT MAX(time_updated) FROM session", [], |r| r.get(0))
                    .unwrap_or(None);
                let bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
                return (n, bytes, p, last);
            }
        }
    }
    let Some(root) = storage_root(home) else { return (0, 0, 0, None) };
    let mut sessions = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    for entry in walkdir::WalkDir::new(root.join("session")).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            sessions += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(m) = mtime_ms(p) {
                last = Some(last.map_or(m, |l: i64| l.max(m)));
            }
        }
    }
    let projects = walkdir::WalkDir::new(root.join("project"))
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count();
    (sessions, bytes, projects, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn sync_residue_is_not_an_install() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".local/share/opencode/storage/session")).unwrap();
        assert!(!detect(home), "storage/ alone is sync residue");
        std::fs::write(home.join(".local/share/opencode/auth.json"), "{}").unwrap();
        assert!(detect(home), "a real install marker flips detection");
    }

    fn make_db(dir: &Path) -> PathBuf {
        let p = dir.join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
               slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL,
               version TEXT NOT NULL, time_created INTEGER NOT NULL,
               time_updated INTEGER NOT NULL);
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
               time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
               sandboxes TEXT NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
               time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
               data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
               session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
               time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        p
    }

    #[test]
    fn db_sessions_roundtrip_and_newest_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let db_a = make_db(&a);
        // Marker so detect() sees a real install on both sides.
        std::fs::write(a.join(".local/share/opencode/auth.json"), "{}").unwrap();
        {
            let c = rusqlite::Connection::open(&db_a).unwrap();
            c.execute_batch(
                "INSERT INTO project VALUES ('prj1', '/Users/u/dev/app', 1, 1, '[]');
                 INSERT INTO session VALUES ('ses1','prj1','test','/Users/u/dev/app','Windows test','1.0',100,200);
                 INSERT INTO message VALUES ('msg1','ses1',100,100,'{\"role\":\"user\"}');
                 INSERT INTO part VALUES ('prt1','msg1','ses1',100,100,'{\"text\":\"hi\"}');",
            )
            .unwrap();
        }
        let tok_a = Tokenizer::with_case_sensitivity("/Users/u", false);
        let mut st_a = SyncState::default();
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a").unwrap(), 1);
        // Re-push with no change: content-hash diff skips.
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a").unwrap(), 0);

        // Machine B (different home): session lands fully, paths expanded.
        let b = tmp.path().join("b");
        let db_b = make_db(&b);
        std::fs::write(b.join(".local/share/opencode/auth.json"), "{}").unwrap();
        let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
        let mut st_b = SyncState::default();
        let listing = store.list().unwrap();
        let r = db_apply(&b, &tok_b, &mut st_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(r.applied, 1);
        let c = rusqlite::Connection::open(&db_b).unwrap();
        let (dir, title): (String, String) = c
            .query_row("SELECT directory, title FROM session WHERE id='ses1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(dir, "/home/bob/dev/app");
        assert_eq!(title, "Windows test");
        let msgs: i64 = c.query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0)).unwrap();
        let parts: i64 = c.query_row("SELECT COUNT(*) FROM part", [], |r| r.get(0)).unwrap();
        assert_eq!((msgs, parts), (1, 1));
        // Backup was taken before the first write.
        assert!(db_b.with_extension("db.vibesync-bak").exists());

        // Local session newer than remote: untouched.
        c.execute("UPDATE session SET title='local edit', time_updated=999 WHERE id='ses1'", [])
            .unwrap();
        let mut st_b2 = SyncState::default();
        let r = db_apply(&b, &tok_b, &mut st_b2, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        let title: String =
            c.query_row("SELECT title FROM session WHERE id='ses1'", [], |r| r.get(0)).unwrap();
        assert_eq!(title, "local edit");
    }

    #[test]
    fn storage_files_sync_additively() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let s = a.join(".local/share/opencode/storage/session");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("ses_1.json"), "{\"id\":\"ses_1\"}").unwrap();
        // A volatile dir that must be skipped.
        let sd = a.join(".local/share/opencode/storage/session_diff");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("d.json"), "x").unwrap();

        let entries = scan(&a).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].logical.ends_with("session/ses_1.json"));
        let mut state_a = SyncState::default();
        crate::sync::push(&entries, &mut state_a, &store, "a").unwrap();

        let b = tmp.path().join("b");
        std::fs::create_dir_all(b.join(".local/share/opencode/storage")).unwrap();
        let mut state_b = SyncState::default();
        let listing = store.list().unwrap();
        let report = apply(&b, &mut state_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(report.pulled, 1);
        assert!(b.join(".local/share/opencode/storage/session/ses_1.json").exists());
    }
}
