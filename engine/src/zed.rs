//! Zed adapter.
//!
//! Storage (validated on macOS 2026-07, re-validated 2026-07-14): agent
//! threads live in the SQLite db `<config>/Zed/threads/threads.db`, one row
//! per thread. The column set DRIFTS across Zed versions (July 2025 builds
//! had `worktree_branch`; July 2026 builds dropped it), and a fleet can run
//! mixed versions — so rows travel as generic column maps, exactly like the
//! Codex/OpenCode db recipe: unknown columns are dropped on insert, missing
//! ones hit the local table's defaults. `data` is a zstd-compressed thread
//! blob — synced opaquely (hex in the store object, real BLOB in the db).
//! `folder_paths` holds absolute paths, tokenized to `${HOME}`.
//!
//! Row-level sync: each thread is a `zed/threads/<id>.json` store object;
//! apply upserts by id with newest `updated_at` (ISO text) winning. Writes
//! use a busy timeout so a running Zed doesn't hard-fail the sync.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
/// Every location this adapter considers — for the transparency trace.
pub fn probe_locations() -> Vec<PathBuf> {
    if let Some(db) = db_path() {
        return vec![db];
    }
    [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
        .map(|d| d.join("Zed").join("threads").join("threads.db"))
        .collect()
}

pub fn detect() -> bool {
    if let Some(db) = db_path() {
        crate::dlog::debug(|| format!("detect zed: installed (threads db at {})", db.display()));
        return true;
    }
    let found = [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
        .into_iter()
        .flatten()
        .any(|d| d.join("Zed").is_dir());
    crate::dlog::debug(|| {
        format!(
            "detect zed: {}",
            if found { "installed (Zed dir, no threads db yet)" } else { "NOT installed" }
        )
    });
    found
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

/// Read all thread rows from `db` as tokenized generic column maps:
/// (id, updated_at, serialized-json). `data` arrives hex-encoded via
/// row_to_map's blob handling.
fn scan_db(db: &Path, home: &Path) -> Result<Vec<(String, String, Vec<u8>)>> {
    let home = home.to_string_lossy();
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let rows = crate::dbsync::query_maps(&conn, "SELECT * FROM threads", &[])?;
    let mut out = Vec::new();
    for mut row in rows {
        let Some(id) = row.get("id").and_then(|v| v.as_str()).map(String::from) else { continue };
        let updated =
            row.get("updated_at").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if let Some(serde_json::Value::String(fp)) = row.get("folder_paths") {
            let t = tokenize(fp, &home);
            row.insert("folder_paths".into(), serde_json::Value::String(t));
        }
        out.push((id, updated, serde_json::to_vec(&row)?));
    }
    Ok(out)
}

pub fn push(home: &Path, state: &mut SyncState, store: &dyn SyncStore, machine: &str) -> Result<usize> {
    let Some(db) = db_path() else { return Ok(0) };
    push_db(&db, home, state, store, machine)
}

fn push_db(
    db: &Path,
    home: &Path,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
) -> Result<usize> {
    let mut pushed = 0;
    for (id, updated, json) in scan_db(db, home)? {
        let hash = hash_bytes(&json);
        let logical = format!("{PREFIX}/{id}.json");
        if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
            continue;
        }
        let mtime_ms = crate::dbsync::iso_ms(&updated);
        store.put(
            &logical,
            &json,
            &RemoteMeta { hash: hash.clone(), mtime_ms, size: json.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms, size: json.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    Ok(pushed)
}

/// Upsert one thread map into the local db, keeping only columns the LOCAL
/// table knows (a sender on a different Zed generation must never fail the
/// insert), and re-materializing `data` from hex to a real BLOB.
fn upsert_thread(
    conn: &rusqlite::Connection,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let local = crate::dbsync::table_cols(conn, "threads")?;
    let cols: Vec<&String> = local.iter().filter(|c| map.contains_key(*c)).collect();
    if !cols.iter().any(|c| *c == "id") {
        anyhow::bail!("thread object without id");
    }
    let names = cols.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(",");
    let placeholders = (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let updates = cols
        .iter()
        .filter(|c| **c != "id")
        .map(|c| format!("`{c}`=excluded.`{c}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO threads ({names}) VALUES ({placeholders})
         ON CONFLICT(id) DO UPDATE SET {updates}"
    );
    let params: Vec<Box<dyn rusqlite::ToSql>> = cols
        .iter()
        .map(|c| -> Box<dyn rusqlite::ToSql> {
            match &map[*c] {
                serde_json::Value::String(s) if *c == "data" => Box::new(hex_decode(s)),
                serde_json::Value::Null => Box::new(rusqlite::types::Null),
                serde_json::Value::Bool(b) => Box::new(*b as i64),
                serde_json::Value::Number(n) if n.is_i64() => Box::new(n.as_i64().unwrap()),
                serde_json::Value::Number(n) => Box::new(n.as_f64().unwrap_or(0.0)),
                serde_json::Value::String(s) => Box::new(s.clone()),
                other => Box::new(other.to_string()),
            }
        })
        .collect();
    conn.execute(&sql, rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())))
        .context("upsert zed thread")?;
    Ok(())
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let Some(db) = db_path() else { return Ok(Default::default()) };
    apply_db(&db, home, state, store, listing, on_file, on_pulled)
}

fn apply_db(
    db: &Path,
    home: &Path,
    state: &mut SyncState,
    store: &dyn SyncStore,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let home_s = home.to_string_lossy().into_owned();
    let conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;

    let prefix = format!("{PREFIX}/");
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
        let Some((json, _)) = store.get(logical)? else { continue };
        let Ok(mut row) =
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&json)
        else {
            continue;
        };
        // v1 wire compat: early exports carried the blob as `data_hex`.
        if let Some(v) = row.remove("data_hex") {
            row.insert("data".into(), v);
        }
        let (Some(id), Some(updated)) = (
            row.get("id").and_then(|v| v.as_str()).map(String::from),
            row.get("updated_at").and_then(|v| v.as_str()).map(String::from),
        ) else {
            continue;
        };
        // Newest updated_at wins (ISO text compares chronologically).
        let local_updated: Option<String> = conn
            .query_row("SELECT updated_at FROM threads WHERE id = ?1", [&id], |r| r.get(0))
            .ok();
        if let Some(local) = &local_updated {
            if local.as_str() >= updated.as_str() {
                report.skipped_newer_local += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                );
                continue;
            }
        }
        if let Some(serde_json::Value::String(fp)) = row.get("folder_paths") {
            let e = expand(fp, &home_s);
            row.insert("folder_paths".into(), serde_json::Value::String(e));
        }
        crate::dlog::debug(|| format!("zed: upserting thread {id}"));
        upsert_thread(&conn, &row)?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }
    Ok(report)
}

/// (thread count, total blob bytes, worktree folders).
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

    /// July-2025-generation schema — includes `worktree_branch`.
    fn make_db_old(dir: &Path) -> PathBuf {
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

    /// July-2026-generation schema — `worktree_branch` dropped (live catch
    /// on ComputerB, 2026-07-14: the hardcoded column list failed the sync).
    fn make_db_new(dir: &Path) -> PathBuf {
        let p = dir.join("threads").join("threads.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, summary TEXT NOT NULL,
             updated_at TEXT NOT NULL, data_type TEXT NOT NULL, data BLOB NOT NULL,
             parent_id TEXT, folder_paths TEXT, folder_paths_order TEXT, created_at TEXT)",
            [],
        )
        .unwrap();
        p
    }

    #[test]
    fn cross_schema_roundtrip_preserves_blob_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));

        // Machine A: OLD schema, thread with a branch column and a blob.
        let a_home = tmp.path().join("a");
        let a_db = make_db_old(&tmp.path().join("a_zed"));
        {
            let conn = rusqlite::Connection::open(&a_db).unwrap();
            conn.execute(
                "INSERT INTO threads VALUES ('t1','Chat','2026-07-14T10:00:00.000000+00:00',
                 'zstd', x'28b52ffd', 'p1', 'main', ?1, NULL, '2026-07-01T00:00:00+00:00')",
                [format!("{}/dev/app", a_home.to_string_lossy())],
            )
            .unwrap();
        }
        let mut st_a = SyncState::default();
        assert_eq!(push_db(&a_db, &a_home, &mut st_a, &store, "a").unwrap(), 1);
        // Store form: tokenized folder path, hex blob, versioned meta.
        let (json, meta) = store.get("zed/threads/t1.json").unwrap().unwrap();
        let text = String::from_utf8(json).unwrap();
        assert!(text.contains("${HOME}/dev/app"), "{text}");
        assert!(text.contains("28b52ffd"), "{text}");
        assert!(meta.mtime_ms > 0);

        // Machine B: NEW schema (no worktree_branch) — the row must land
        // with the blob byte-identical and the unknown column dropped.
        let b_home = tmp.path().join("b");
        let b_db = make_db_new(&tmp.path().join("b_zed"));
        let mut st_b = SyncState::default();
        let r = apply_db(&b_db, &b_home, &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let conn = rusqlite::Connection::open(&b_db).unwrap();
        let (blob, fp): (Vec<u8>, String) = conn
            .query_row("SELECT data, folder_paths FROM threads WHERE id='t1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(blob, vec![0x28, 0xb5, 0x2f, 0xfd], "blob must survive byte-for-byte");
        assert_eq!(fp, format!("{}/dev/app", b_home.to_string_lossy()));

        // Reverse: B's copy pushes back and the OLD schema accepts it
        // (missing worktree_branch hits the column default).
        let mut st_b2 = SyncState::default();
        assert_eq!(push_db(&b_db, &b_home, &mut st_b2, &store, "b").unwrap(), 1);
        let a2_db = make_db_old(&tmp.path().join("a2_zed"));
        let mut st_a2 = SyncState::default();
        let r = apply_db(&a2_db, &a_home, &mut st_a2, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
    }

    #[test]
    fn newer_local_thread_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a_db = make_db_new(&tmp.path().join("a_zed"));
        {
            let conn = rusqlite::Connection::open(&a_db).unwrap();
            conn.execute(
                "INSERT INTO threads (id, summary, updated_at, data_type, data)
                 VALUES ('t2','old remote','2026-07-10T00:00:00+00:00','zstd', x'01')",
                [],
            )
            .unwrap();
        }
        let mut st = SyncState::default();
        push_db(&a_db, tmp.path(), &mut st, &store, "a").unwrap();

        let b_db = make_db_new(&tmp.path().join("b_zed"));
        {
            let conn = rusqlite::Connection::open(&b_db).unwrap();
            conn.execute(
                "INSERT INTO threads (id, summary, updated_at, data_type, data)
                 VALUES ('t2','newer local','2026-07-14T00:00:00+00:00','zstd', x'02')",
                [],
            )
            .unwrap();
        }
        let mut st_b = SyncState::default();
        let r = apply_db(&b_db, tmp.path(), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        let s: String = rusqlite::Connection::open(&b_db)
            .unwrap()
            .query_row("SELECT summary FROM threads WHERE id='t2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "newer local");
    }

    #[test]
    fn v1_wire_objects_still_apply() {
        // Early exports carried the blob under `data_hex`.
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let v1 = serde_json::json!({
            "id": "t3", "summary": "legacy", "updated_at": "2026-07-14T00:00:00+00:00",
            "data_type": "zstd", "data_hex": "0a0b", "parent_id": null,
            "worktree_branch": null, "folder_paths": null,
            "folder_paths_order": null, "created_at": null
        });
        let bytes = serde_json::to_vec(&v1).unwrap();
        store
            .put(
                "zed/threads/t3.json",
                &bytes,
                &RemoteMeta { hash: hash_bytes(&bytes), mtime_ms: 1, size: bytes.len() as u64, source: "a".into() },
            )
            .unwrap();
        let db = make_db_new(&tmp.path().join("zed"));
        let mut st = SyncState::default();
        let r = apply_db(&db, tmp.path(), &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let blob: Vec<u8> = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row("SELECT data FROM threads WHERE id='t3'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob, vec![0x0a, 0x0b]);
    }
}
