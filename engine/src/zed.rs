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
//! `folder_paths` holds absolute paths (one per line), tokenized per path
//! with the shared boundary-aware Tokenizer.
//!
//! Row-level sync: each thread is a `zed/threads/<id>.json` store object;
//! apply upserts by id with newest `updated_at` (ISO text) winning. Writes
//! use a busy timeout so a running Zed doesn't hard-fail the sync, follow a
//! one-time consistent backup (`threads.db.vibesync-bak`), and a thread
//! that can't be merged is skipped and counted, never fatal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

use crate::scanner::hash_bytes;
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};
use crate::tokenizer::Tokenizer;

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

/// `folder_paths` is Zed's serialized path list — one absolute path per
/// line — so each line translates on its own, through the same
/// boundary-aware Tokenizer every other adapter uses. The raw substring
/// replace this replaces turned `/Users/alice` into `${HOME}ice` on a
/// machine whose home is `/Users/al`, never normalized separators across
/// OSes, ignored Windows case, and knew nothing of `${GIT:...}`.
fn tokenize_folder_paths(s: &str, tok: &Tokenizer) -> String {
    s.split('\n').map(|p| tok.tokenize_plain(p)).collect::<Vec<_>>().join("\n")
}

fn expand_folder_paths(s: &str, tok: &Tokenizer) -> String {
    s.split('\n').map(|p| crate::dbsync::expand_path(p, tok)).collect::<Vec<_>>().join("\n")
}

/// Read all thread rows from `db` as tokenized generic column maps:
/// (id, updated_at, serialized-json). `data` arrives hex-encoded via
/// row_to_map's blob handling.
fn scan_db(db: &Path, tok: &Tokenizer) -> Result<Vec<(String, String, Vec<u8>)>> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    if !crate::dbsync::table_exists(&conn, "threads") {
        return Ok(Vec::new()); // a Zed that never used the agent yet
    }
    let rows = crate::dbsync::query_maps(&conn, "SELECT * FROM threads", &[])?;
    let mut out = Vec::new();
    for mut row in rows {
        let Some(id) = row.get("id").and_then(|v| v.as_str()).map(String::from) else { continue };
        let updated =
            row.get("updated_at").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        if let Some(serde_json::Value::String(fp)) = row.get("folder_paths") {
            let t = tokenize_folder_paths(fp, tok);
            row.insert("folder_paths".into(), serde_json::Value::String(t));
        }
        out.push((id, updated, serde_json::to_vec(&row)?));
    }
    Ok(out)
}

/// Publish this machine's threads; paths tokenize against `home` alone
/// (`${HOME}`), which every VibeSync version can expand.
pub fn push(
    home: &Path,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    push_with_tokenizer(&Tokenizer::new(&home.to_string_lossy()), state, store, machine, listing)
}

/// [`push`] with the caller's full Tokenizer (git/project mappings), so a
/// thread's folders can travel as `${GIT:...}` like other adapters' paths.
pub fn push_with_tokenizer(
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    let Some(db) = db_path() else { return Ok(0) };
    push_db(&db, tok, state, store, machine, listing)
}

/// Two memories per thread, in separate state slots (the Codex one-shot
/// re-publish rule): the store version SEEN (main slot, written by apply)
/// and our OWN canonical serialization (#own slot, written here). Machines
/// on different Zed schema generations never serialize identically, so
/// diffing publish against the apply-recorded hash re-exported every
/// thread after every apply, forever — live-caught on the first Windows
/// round (same thread uploaded on consecutive syncs from both sides).
fn push_db(
    db: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    let prefix = format!("{PREFIX}/");
    let remote: std::collections::HashMap<&str, (&str, i64)> = listing
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, m)| (k.as_str(), (m.hash.as_str(), m.mtime_ms)))
        .collect();
    let mut pushed = 0;
    for (id, updated, json) in scan_db(db, tok)? {
        let hash = hash_bytes(&json);
        let logical = format!("{PREFIX}/{id}.json");
        let eff = crate::dbsync::iso_ms(&updated);
        let own_slot = format!("{logical}#own");
        let own_prev = state.files.get(&own_slot).map(|s| s.hash == hash).unwrap_or(false)
            || state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false);
        state.files.insert(
            own_slot,
            FileState { hash: hash.clone(), mtime_ms: eff, size: 0, deleted_locally: false },
        );
        // A queued self-heal re-upload is sent whatever the store holds.
        let force = state.reupload_pending(&logical);
        if let (false, Some((rhash, rmtime))) = (force, remote.get(logical.as_str())) {
            // A strictly newer store copy always wins; at EQUAL version we
            // re-publish only when our own canonical bytes changed since
            // we last pushed — a deterministic local fact, so cross-schema
            // drift settles in one bounded hop instead of ping-ponging.
            if *rmtime > 0 && (*rhash == hash || *rmtime > eff || (*rmtime >= eff && own_prev)) {
                continue;
            }
        } else if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
            continue;
        }
        store.put(
            &logical,
            &json,
            &RemoteMeta { hash: hash.clone(), mtime_ms: eff, size: json.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms: eff, size: json.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    Ok(pushed)
}

/// Upsert one thread map into the local db, keeping only columns the LOCAL
/// table knows (a sender on a different Zed generation must never fail the
/// insert), and re-materializing `data` from hex to a real BLOB. The upsert
/// is `ON CONFLICT DO UPDATE` — an in-place update, never a delete — and
/// leaves columns the sender doesn't carry untouched.
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
    let params = cols
        .iter()
        .map(|c| -> Result<Box<dyn rusqlite::ToSql>> {
            Ok(match &map[*c] {
                // The thread itself: bytes that aren't valid hex are a
                // damaged export — refuse it rather than write a blob of
                // invented 0x00s over the thread.
                serde_json::Value::String(s) if *c == "data" => Box::new(
                    crate::dbsync::hex_decode(s)
                        .ok_or_else(|| anyhow::anyhow!("thread blob is not valid hex"))?,
                ),
                serde_json::Value::Null => Box::new(rusqlite::types::Null),
                serde_json::Value::Bool(b) => Box::new(*b as i64),
                serde_json::Value::Number(n) if n.is_i64() => Box::new(n.as_i64().unwrap()),
                serde_json::Value::Number(n) => Box::new(n.as_f64().unwrap_or(0.0)),
                serde_json::Value::String(s) => Box::new(s.clone()),
                other => Box::new(other.to_string()),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    conn.execute(&sql, rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())))
        .context("upsert zed thread")?;
    Ok(())
}

/// Merge other machines' threads; paths expand against `home` alone.
pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    apply_with_tokenizer(&Tokenizer::new(&home.to_string_lossy()), state, store, listing, on_file, on_pulled)
}

/// [`apply`] with the caller's full Tokenizer: `${GIT:...}` folders expand
/// to this machine's clone (a repo unknown here parks the thread).
pub fn apply_with_tokenizer(tok: &Tokenizer, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let Some(db) = db_path() else { return Ok(Default::default()) };
    apply_db(&db, tok, state, store, listing, on_file, on_pulled)
}

fn apply_db(
    db: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let conn = crate::dbsync::open_rw(db)?;
    let mut backed_up = false;

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
        let Some(json) = crate::sync::fetch_obj(store, logical, &mut report.failed) else { continue };
        // From here on, a problem with this thread costs this thread only:
        // skipped, counted, untracked so the next sync retries it.
        let Ok(mut row) =
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&json)
        else {
            crate::dlog::warn(|| format!("zed: {logical} is not a readable thread export — skipping"));
            report.failed += 1;
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
            crate::dlog::warn(|| format!("zed: {logical} lacks a thread id/updated_at — skipping"));
            report.failed += 1;
            continue;
        };
        // Newest updated_at wins (ISO text compares chronologically).
        let local_updated = conn
            .query_row("SELECT updated_at FROM threads WHERE id = ?1", [&id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional();
        let local_updated = match local_updated {
            Ok(v) => v.flatten(),
            Err(e) => {
                crate::dlog::warn(|| format!("zed: cannot read local thread {id}: {e:#} — skipping"));
                report.failed += 1;
                continue;
            }
        };
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
            let e = expand_folder_paths(fp, tok);
            if crate::gitmap::has_unresolved_token(&e) {
                crate::dlog::debug(|| format!("zed: {logical} parked — project not on this machine"));
                report.parked += 1; // retried once the repo appears here
                continue;
            }
            row.insert("folder_paths".into(), serde_json::Value::String(e));
        }
        // The one-time backup precedes the first-ever write; without it,
        // nothing is written this sync.
        if !backed_up {
            crate::dbsync::backup_db_once(&conn, db).with_context(|| {
                format!("cannot back up {} before writing to it — nothing merged this sync", db.display())
            })?;
            backed_up = true;
        }
        crate::dlog::debug(|| format!("zed: upserting thread {id}"));
        if let Err(e) = upsert_thread(&conn, &row) {
            crate::dlog::warn(|| format!("zed: thread {id} not merged: {e:#} — retried next sync"));
            report.failed += 1;
            continue;
        }
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

    fn tok(home: &Path) -> Tokenizer {
        Tokenizer::with_case_sensitivity(&home.to_string_lossy(), false)
    }

    fn put_thread(store: &FolderStore, v: serde_json::Value) {
        let bytes = serde_json::to_vec(&v).unwrap();
        store
            .put(
                &format!("zed/threads/{}.json", v["id"].as_str().unwrap()),
                &bytes,
                &RemoteMeta { hash: hash_bytes(&bytes), mtime_ms: 1, size: bytes.len() as u64, source: "a".into() },
            )
            .unwrap();
    }

    fn thread(id: &str, folder_paths: Option<&str>, data_hex: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "summary": "s", "updated_at": "2026-07-14T00:00:00+00:00",
            "data_type": "zstd", "data": data_hex, "parent_id": null,
            "folder_paths": folder_paths, "folder_paths_order": null, "created_at": null
        })
    }

    #[test]
    fn folder_paths_translate_per_path_on_component_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        // Home `/Users/al`: a sibling user's `/Users/alice/x` must NOT become
        // `${HOME}ice/x`, and every line of the path list translates alone.
        let a_db = make_db_new(&tmp.path().join("a_zed"));
        rusqlite::Connection::open(&a_db)
            .unwrap()
            .execute(
                "INSERT INTO threads (id, summary, updated_at, data_type, data, folder_paths)
                 VALUES ('t5', 's', '2026-07-14T00:00:00+00:00', 'zstd', x'01',
                         '/Users/alice/x' || char(10) || '/Users/al/dev/app')",
                [],
            )
            .unwrap();
        let a = Tokenizer::with_case_sensitivity("/Users/al", false);
        push_db(&a_db, &a, &mut SyncState::default(), &store, "a", &[]).unwrap();
        let (json, _) = store.get("zed/threads/t5.json").unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["folder_paths"], "/Users/alice/x\n${HOME}/dev/app");

        // A Windows machine's export (backslash tail) lands with this
        // machine's separators; an unknown repo token parks the thread.
        put_thread(&store, thread("t6", Some("${HOME}\\dev\\win"), "02"));
        put_thread(&store, thread("t7", Some("${GIT:github.com:o:elsewhere}/x"), "03"));
        let b_db = make_db_new(&tmp.path().join("b_zed"));
        let b = Tokenizer::with_case_sensitivity("/home/al", false);
        let r = apply_db(&b_db, &b, &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!((r.applied, r.parked), (2, 1));
        let fp = |id: &str| -> String {
            rusqlite::Connection::open(&b_db)
                .unwrap()
                .query_row("SELECT folder_paths FROM threads WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(fp("t5"), "/Users/alice/x\n/home/al/dev/app");
        assert_eq!(fp("t6"), "/home/al/dev/win");
    }

    #[test]
    fn damaged_blob_fails_only_that_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        put_thread(&store, thread("bad1", None, "0é0f")); // multi-byte: used to panic
        put_thread(&store, thread("bad2", None, "zz")); // not hex: used to write 0x00
        put_thread(&store, thread("good", None, "28b5"));
        let db = make_db_new(&tmp.path().join("zed"));
        let mut st = SyncState::default();
        let r = apply_db(&db, &tok(tmp.path()), &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!((r.applied, r.failed), (1, 2));
        let ids: Vec<String> = rusqlite::Connection::open(&db)
            .unwrap()
            .prepare("SELECT id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids, ["good"]);
        assert!(!st.files.contains_key("zed/threads/bad1.json"), "untracked, so it retries");
    }

    /// A thread export queued for a self-heal re-upload goes out even
    /// though the store's copy has the same hash, then settles.
    #[test]
    fn a_thread_queued_for_reupload_is_resent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let db = make_db_new(&tmp.path().join("zed"));
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO threads (id, summary, updated_at, data_type, data)
                 VALUES ('t1', 's', '2026-07-01T00:00:00+00:00', 'zstd', x'09');",
            )
            .unwrap();
        let t = tok(tmp.path());
        let mut st = SyncState::default();
        let push = |st: &mut SyncState| push_db(&db, &t, st, &store, "a", &store.list().unwrap()).unwrap();
        assert_eq!(push(&mut st), 1);
        assert_eq!(push(&mut st), 0, "settled");
        crate::sync::mark_for_reupload(&mut st, &[format!("{PREFIX}/t1.json")]);
        assert_eq!(push(&mut st), 1, "re-sent");
        assert_eq!(push(&mut st), 0);
    }

    #[test]
    fn first_write_takes_a_consistent_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        put_thread(&store, thread("t8", None, "01"));
        let db = make_db_new(&tmp.path().join("zed"));
        // Zed is running: its own thread is still only in the WAL.
        let live = rusqlite::Connection::open(&db).unwrap();
        live.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;
             INSERT INTO threads (id, summary, updated_at, data_type, data)
             VALUES ('mine', 's', '2026-07-01T00:00:00+00:00', 'zstd', x'09');",
        )
        .unwrap();
        let r = apply_db(&db, &tok(tmp.path()), &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let bak = db.with_file_name("threads.db.vibesync-bak");
        let ids: Vec<String> = rusqlite::Connection::open(&bak)
            .unwrap()
            .prepare("SELECT id FROM threads")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids, ["mine"], "pre-merge state, WAL content included");
        drop(live);
    }

    #[test]
    fn a_failing_thread_does_not_stop_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        put_thread(&store, thread("t9", None, "01"));
        put_thread(&store, thread("t10", None, "02"));
        let db = make_db_new(&tmp.path().join("zed"));
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON threads WHEN NEW.id = 't9'
                 BEGIN SELECT RAISE(ABORT, 'locked'); END;",
            )
            .unwrap();
        let r = apply_db(&db, &tok(tmp.path()), &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!((r.applied, r.failed), (1, 1));
    }

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
        assert_eq!(push_db(&a_db, &tok(&a_home), &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 1);
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
        let r = apply_db(&b_db, &tok(&b_home), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let conn = rusqlite::Connection::open(&b_db).unwrap();
        let (blob, fp): (Vec<u8>, String) = conn
            .query_row("SELECT data, folder_paths FROM threads WHERE id='t1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(blob, vec![0x28, 0xb5, 0x2f, 0xfd], "blob must survive byte-for-byte");
        // A real path on THIS machine, native separators included (the old
        // raw substring replace produced `C:\\...\\b/dev/app` on Windows).
        assert_eq!(fp, b_home.join("dev").join("app").to_string_lossy());

        // Reverse: B's copy pushes back and the OLD schema accepts it
        // (missing worktree_branch hits the column default).
        let mut st_b2 = SyncState::default();
        assert_eq!(push_db(&b_db, &tok(&b_home), &mut st_b2, &store, "b", &store.list().unwrap()).unwrap(), 1);
        let a2_db = make_db_old(&tmp.path().join("a2_zed"));
        let mut st_a2 = SyncState::default();
        let r = apply_db(&a2_db, &tok(&a_home), &mut st_a2, &store, &store.list().unwrap(), &|| {}, &|_| {})
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
        push_db(&a_db, &tok(tmp.path()), &mut st, &store, "a", &store.list().unwrap()).unwrap();

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
        let r = apply_db(&b_db, &tok(tmp.path()), &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        let s: String = rusqlite::Connection::open(&b_db)
            .unwrap()
            .query_row("SELECT summary FROM threads WHERE id='t2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s, "newer local");
    }

    #[test]
    fn cross_schema_republish_settles_in_one_hop() {
        // Live catch (Windows, 2026-07-14): after applying B's thread,
        // Windows re-exported its own serialization every sync, and B
        // mirrored it back — the codex ping-pong, reborn in zed.
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a_db = make_db_old(&tmp.path().join("a_zed"));
        {
            let conn = rusqlite::Connection::open(&a_db).unwrap();
            conn.execute(
                "INSERT INTO threads VALUES ('t4','Chat','2026-07-14T10:00:00+00:00',
                 'zstd', x'0102', NULL, 'main', NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        }
        let t = tok(tmp.path());
        let mut st_a = SyncState::default();
        assert_eq!(push_db(&a_db, &t, &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 1);

        // B (different schema) applies, then re-publishes its own
        // serialization ONCE — the bounded hop.
        let b_db = make_db_new(&tmp.path().join("b_zed"));
        let mut st_b = SyncState::default();
        apply_db(&b_db, &t, &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(
            push_db(&b_db, &t, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(),
            1,
            "first cross-schema re-publish is the one allowed hop"
        );
        let (_, meta_b) = store.get("zed/threads/t4.json").unwrap().unwrap();

        // A sees B's copy: apply keeps the local row (equal version),
        // records the store version — and its next publish must NOT push
        // its own serialization back (own bytes unchanged since its push).
        let r = apply_db(&a_db, &t, &mut st_a, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        assert_eq!(
            push_db(&a_db, &t, &mut st_a, &store, "a", &store.list().unwrap()).unwrap(),
            0,
            "equal version + unchanged own bytes must settle, not ping-pong"
        );
        // And B stays settled too.
        apply_db(&b_db, &t, &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(
            push_db(&b_db, &t, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(),
            0
        );
        let (_, meta_after) = store.get("zed/threads/t4.json").unwrap().unwrap();
        assert_eq!(meta_after.hash, meta_b.hash, "store object stops churning");
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
        let r = apply_db(&db, &tok(tmp.path()), &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let blob: Vec<u8> = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row("SELECT data FROM threads WHERE id='t3'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(blob, vec![0x0a, 0x0b]);
    }
}
