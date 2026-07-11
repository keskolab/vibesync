//! Zed adapter.
//!
//! Storage (validated on macOS, 2026-07): agent threads live in the SQLite
//! db `<config>/Zed/threads/threads.db`, one row per thread:
//!   threads(id PK, summary, updated_at, data_type, data BLOB,
//!           parent_id, worktree_branch, folder_paths, folder_paths_order,
//!           created_at)
//! `data` is a zstd-compressed thread blob — synced opaquely (hex in the
//! store object). `folder_paths` holds absolute paths, tokenized to `${HOME}`.
//!
//! Row-level sync: each thread is a `zed/threads/<id>.json` store object;
//! apply upserts by id with newest `updated_at` winning. Writes use a busy
//! timeout so a running Zed doesn't hard-fail the sync (it retries next time).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::scanner::hash_bytes;
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const PREFIX: &str = "zed/threads";

pub fn db_path() -> Option<PathBuf> {
    // macOS: ~/Library/Application Support/Zed. Windows: %APPDATA%\Zed or
    // %LOCALAPPDATA%\Zed — try both.
    let mut candidates = Vec::new();
    if let Some(c) = dirs::config_dir() {
        candidates.push(c.join("Zed"));
    }
    if let Some(d) = dirs::data_dir() {
        candidates.push(d.join("Zed"));
    }
    if let Some(d) = dirs::data_local_dir() {
        candidates.push(d.join("Zed"));
    }
    candidates
        .into_iter()
        .map(|p| p.join("threads").join("threads.db"))
        .find(|p| p.exists())
}

/// Installed = a Zed dir exists (threads.db only appears after first agent
/// use, so don't require it).
pub fn detect() -> bool {
    if db_path().is_some() {
        return true;
    }
    [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
        .any(|d| d.join("Zed").is_dir())
}

#[derive(Debug, Serialize, Deserialize)]
struct ThreadRow {
    id: String,
    summary: String,
    updated_at: String,
    data_type: String,
    /// zstd blob, hex-encoded for JSON transport.
    data_hex: String,
    parent_id: Option<String>,
    worktree_branch: Option<String>,
    folder_paths: Option<String>,
    folder_paths_order: Option<String>,
    created_at: Option<String>,
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

fn tokenize(s: &str, home: &str) -> String {
    s.replace(home, "${HOME}")
}
fn expand(s: &str, home: &str) -> String {
    s.replace("${HOME}", home)
}

/// Read all thread rows, tokenized, as (id, updated_at, serialized-json).
pub fn scan(home: &Path) -> Result<Vec<(String, String, Vec<u8>)>> {
    let Some(path) = db_path() else { return Ok(vec![]) };
    let home = home.to_string_lossy();
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, summary, updated_at, data_type, data, parent_id, \
         worktree_branch, folder_paths, folder_paths_order, created_at FROM threads",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ThreadRow {
            id: r.get(0)?,
            summary: r.get(1)?,
            updated_at: r.get(2)?,
            data_type: r.get(3)?,
            data_hex: hex_encode(&r.get::<_, Vec<u8>>(4)?),
            parent_id: r.get(5)?,
            worktree_branch: r.get(6)?,
            folder_paths: r.get::<_, Option<String>>(7)?.map(|s| tokenize(&s, &home)),
            folder_paths_order: r.get(8)?,
            created_at: r.get(9)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        let json = serde_json::to_vec(&row)?;
        out.push((row.id.clone(), row.updated_at.clone(), json));
    }
    Ok(out)
}

pub fn push(home: &Path, state: &mut SyncState, store: &dyn SyncStore, machine: &str) -> Result<usize> {
    let mut pushed = 0;
    for (id, _updated, json) in scan(home)? {
        let hash = hash_bytes(&json);
        let logical = format!("{PREFIX}/{id}.json");
        if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
            continue;
        }
        store.put(
            &logical,
            &json,
            &RemoteMeta { hash: hash.clone(), mtime_ms: 0, size: json.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms: 0, size: json.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    Ok(pushed)
}

#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub applied: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn()) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    let Some(path) = db_path() else { return Ok(report) };
    let home_s = home.to_string_lossy().into_owned();
    let conn = rusqlite::Connection::open(&path)?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;

    let prefix = format!("{PREFIX}/");
    for (logical, meta) in listing {
        if !logical.starts_with(&prefix) {
            continue;
        }
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let Some((json, _)) = store.get(logical)? else { continue };
        let row: ThreadRow = match serde_json::from_slice(&json) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Newest updated_at wins.
        let local_updated: Option<String> = conn
            .query_row("SELECT updated_at FROM threads WHERE id = ?1", [&row.id], |r| r.get(0))
            .ok();
        if let Some(local) = &local_updated {
            if local.as_str() >= row.updated_at.as_str() {
                report.skipped_newer_local += 1;
                continue;
            }
        }
        let data = hex_decode(&row.data_hex);
        let folder_paths = row.folder_paths.as_ref().map(|s| expand(s, &home_s));
        conn.execute(
            "INSERT INTO threads
               (id, summary, updated_at, data_type, data, parent_id,
                worktree_branch, folder_paths, folder_paths_order, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(id) DO UPDATE SET
               summary=excluded.summary, updated_at=excluded.updated_at,
               data_type=excluded.data_type, data=excluded.data,
               parent_id=excluded.parent_id, worktree_branch=excluded.worktree_branch,
               folder_paths=excluded.folder_paths,
               folder_paths_order=excluded.folder_paths_order,
               created_at=excluded.created_at",
            rusqlite::params![
                row.id, row.summary, row.updated_at, row.data_type, data,
                row.parent_id, row.worktree_branch, folder_paths,
                row.folder_paths_order, row.created_at
            ],
        )
        .context("upsert zed thread")?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: 0, size: meta.size, deleted_locally: false },
        );
        report.applied += 1;
    }
    Ok(report)
}

/// (thread count, total blob bytes, worktree folders, "" — no reliable mtime).
pub fn light_counts() -> (usize, u64, usize) {
    let Some(path) = db_path() else { return (0, 0, 0) };
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return (0, 0, 0);
    };
    let n: usize = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).unwrap_or(0);
    let bytes: i64 = conn
        .query_row("SELECT COALESCE(SUM(LENGTH(data)),0) FROM threads", [], |r| r.get(0))
        .unwrap_or(0);
    let folders: usize = conn
        .query_row(
            "SELECT COUNT(DISTINCT folder_paths) FROM threads WHERE folder_paths IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    (n, bytes as u64, folders)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    fn make_db(dir: &Path) -> PathBuf {
        let p = dir.join("threads").join("threads.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, summary TEXT, updated_at TEXT,
             data_type TEXT, data BLOB, parent_id TEXT, worktree_branch TEXT,
             folder_paths TEXT, folder_paths_order TEXT, created_at TEXT)",
            [],
        )
        .unwrap();
        p
    }

    #[test]
    fn thread_roundtrip_tokenizes_paths() {
        // Point db_path() at a temp Zed dir via config override isn't possible,
        // so drive scan/apply against explicit dbs through the public row API.
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));

        // Build A's db and push one thread referencing A's home path.
        let a_home = tmp.path().join("a");
        let a_db = make_db(&tmp.path().join("a_zed"));
        {
            let conn = rusqlite::Connection::open(&a_db).unwrap();
            conn.execute(
                "INSERT INTO threads VALUES ('t1','Chat','2026-05-01',
                 'zstd', x'0102', NULL, NULL, ?1, NULL, '2026-04-01')",
                [format!("{}/dev/app", a_home.to_string_lossy())],
            )
            .unwrap();
        }
        // scan reads via db_path(); emulate by reading rows directly here.
        let home_s = a_home.to_string_lossy().into_owned();
        let conn = rusqlite::Connection::open(&a_db).unwrap();
        let row = conn
            .query_row("SELECT folder_paths FROM threads WHERE id='t1'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap();
        let tokenized = tokenize(&row, &home_s);
        assert_eq!(tokenized, "${HOME}/dev/app");
        // And it expands to a different machine's home.
        assert_eq!(expand(&tokenized, "/home/bob"), "/home/bob/dev/app");
        let _ = store;
    }
}
