//! OpenCode adapter (file layer).
//!
//! Storage (validated on macOS, 2026-07): OpenCode keeps per-record JSON
//! under `~/.local/share/opencode/storage/{session,message,part,...}/` AND a
//! relational SQLite db (`opencode.db`). Which is authoritative in the
//! current build is unconfirmed, so this adapter syncs ONLY the storage/
//! files (opaque, additive — records are id-keyed and never collide) and
//! never writes the db. That guarantees a safe cross-machine archive.
//!
//! Three layers, all synced (OpenCode's storage moved twice; a mixed-
//! version fleet has machines on each generation):
//! - storage/ per-record JSON files (oldest layer, additive, opaque).
//! - project/<slug>/storage/ per-project JSON files (current layout per
//!   opencode.ai/docs/troubleshooting; slug is "global" for non-git dirs,
//!   so ad-hoc sessions share a stable key across machines).
//! - opencode.db rows (db-era layer — those builds write sessions only
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
/// Store namespace for the per-project layout (`project/<slug>/storage/`).
pub const PROJECT_PREFIX: &str = "opencode/project";
/// Volatile/derived subdirs we never sync.
const SKIP: &[&str] = &["session_diff", "session_share", "migration"];

/// OpenCode's data root (`~/.local/share/opencode` everywhere in practice;
/// platform data dirs checked as fallback).
fn candidate_roots(home: &Path) -> Vec<PathBuf> {
    let mut cands = vec![home.join(".local/share/opencode")];
    for d in [dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten() {
        let c = d.join("opencode");
        if !cands.contains(&c) {
            cands.push(c);
        }
    }
    cands
}

fn data_root(home: &Path) -> Option<PathBuf> {
    candidate_roots(home).into_iter().find(|c| c.is_dir())
}

/// Every location this adapter considers — for the transparency trace.
pub fn probe_locations(home: &Path) -> Vec<PathBuf> {
    match data_root(home) {
        Some(root) => {
            vec![root.join("opencode.db"), root.join("project"), root.join("storage"), root]
        }
        None => {
            let mut v = vec![home.join(".local/share/opencode")];
            for d in [dirs::data_dir(), dirs::data_local_dir()].into_iter().flatten() {
                v.push(d.join("opencode"));
            }
            v
        }
    }
}

fn storage_root(home: &Path) -> Option<PathBuf> {
    data_root(home).map(|r| r.join("storage")).filter(|s| s.is_dir())
}

/// Installed = the data root holds something a real install writes (db,
/// auth, bin, logs). Pre-gating VibeSync versions created storage/ on
/// machines without OpenCode — such residue must not read as an install.
pub fn detect(home: &Path) -> bool {
    let Some(root) = data_root(home) else {
        crate::dlog::debug(|| {
            "detect opencode: NOT installed (no data root under ~/.local/share or platform data dirs)"
                .to_string()
        });
        return false;
    };
    let Ok(rd) = std::fs::read_dir(&root) else { return false };
    let found = rd.flatten().any(|e| e.file_name() != "storage");
    crate::dlog::debug(|| {
        format!(
            "detect opencode: {} ({})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            root.display()
        )
    });
    found
}

pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let Some(root) = data_root(home) else { return Ok(vec![]) };
    let mut out = Vec::new();
    scan_tree(&root.join("storage"), PREFIX, &mut out)?;
    // Current layout: one storage/ subtree per project slug.
    let projects = root.join("project");
    if projects.is_dir() {
        for e in std::fs::read_dir(&projects)?.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let slug = e.file_name().to_string_lossy().into_owned();
            scan_tree(&e.path().join("storage"), &format!("{PROJECT_PREFIX}/{slug}/storage"), &mut out)?;
        }
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

fn scan_tree(root: &Path, prefix: &str, out: &mut Vec<FileEntry>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
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
            .strip_prefix(root)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(FileEntry {
            logical: format!("{prefix}/{rel}"),
            abs: path.to_path_buf(),
            size: entry.metadata()?.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    Ok(())
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    // Apply even if the dirs don't exist yet (first pull creates them).
    let base = data_root(home).unwrap_or_else(|| home.join(".local/share/opencode"));
    let sprefix = format!("{PREFIX}/");
    let pprefix = format!("{PROJECT_PREFIX}/");
    for (logical, meta) in listing {
        // Both file layers mirror 1:1 under the data root.
        let rel = if let Some(rest) = logical.strip_prefix(&sprefix) {
            format!("storage/{rest}")
        } else if let Some(rest) = logical.strip_prefix(&pprefix) {
            format!("project/{rest}")
        } else {
            continue;
        };
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let mut abs = base.clone();
        for c in rel.split('/') {
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
        report.applied += 1;
    }
    Ok(report)
}

pub const DB_PREFIX: &str = "opencode/db";

/// The max time_created/time_updated across a set of rows. A session's
/// export version is this max over the session row AND every message/part —
/// session.time_updated alone misses writes that only touch child rows (a
/// late message, a streamed part finalizing), and those must still travel.
fn max_time_of<'a>(rows: impl Iterator<Item = &'a serde_json::Map<String, serde_json::Value>>) -> i64 {
    rows.flat_map(|r| {
        ["time_created", "time_updated"]
            .into_iter()
            .filter_map(|k| r.get(k).and_then(|v| v.as_i64()))
    })
    .max()
    .unwrap_or(0)
}

fn max_time_of_json(rows: Option<&serde_json::Value>) -> i64 {
    max_time_of(
        rows.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_object()),
    )
}

fn db_path(home: &Path) -> Option<PathBuf> {
    data_root(home).map(|r| r.join("opencode.db")).filter(|p| p.exists())
}

/// Every directory OpenCode data anchors to on this machine (session
/// directories + project worktrees). Fed to the project map so repos used
/// with OpenCode tokenize as ${GIT:...} and follow the repo across machines
/// regardless of where each clone lives.
pub fn local_dirs(home: &Path) -> Vec<PathBuf> {
    let Some(db) = db_path(home) else { return vec![] };
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return vec![];
    };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
    let mut out: Vec<PathBuf> = Vec::new();
    for sql in ["SELECT DISTINCT directory FROM session", "SELECT DISTINCT worktree FROM project"] {
        if let Ok(mut st) = conn.prepare(sql) {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten().map(PathBuf::from).filter(|p| p.is_dir()));
            }
        }
    }
    out
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

/// Export every db session as one store object, diffed against the store
/// LISTING (not just local state) — a store that lost or never received an
/// export gets it again, so a stale state file can never suppress a push.
pub fn db_push(
    home: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    machine: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<usize> {
    let Some(db) = db_path(home) else {
        crate::dlog::warn(|| "opencode db: no opencode.db found — nothing to export".to_string());
        return Ok(0);
    };
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    // Short-circuit: exporting every session (plus messages/parts) each sync
    // is wasted work when nothing moved. "Nothing moved" must cover BOTH
    // sides: one summary row for the local db, plus the store's current
    // opencode/db object set — so store-side changes re-trigger a full pass.
    let summary: (i64, i64, i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM session),
                (SELECT COALESCE(MAX(time_updated),0) FROM session),
                (SELECT COUNT(*) FROM message),
                (SELECT COUNT(*) FROM part)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let db_kb = std::fs::metadata(&db).map(|m| m.len() / 1024).unwrap_or(0);
    crate::dlog::debug(|| {
        format!(
            "opencode db: {} ({} KB) — {} sessions, {} messages, {} parts",
            db.display(),
            db_kb,
            summary.0,
            summary.2,
            summary.3
        )
    });
    // The db this machine's OpenCode actually writes may live elsewhere
    // (version/platform differences), so every candidate root is probed on
    // every export pass — a single remote debug.log answers "where are the
    // sessions?". Loud when the chosen db looks wrong, quiet otherwise, and
    // skipped entirely when no one is listening (the probe's only product
    // is log lines).
    if summary.0 == 0 {
        crate::dlog::warn(|| format!("opencode db: 0 sessions in {} — is this the db your OpenCode writes?", db.display()));
    }
    let probe_level =
        if summary.0 == 0 { crate::dlog::Level::Warning } else { crate::dlog::Level::Debug };
    if crate::dlog::is_active(probe_level) {
        for cand in candidate_roots(home) {
            let cdb = cand.join("opencode.db");
            let verdict = if !cdb.exists() {
                "no opencode.db".to_string()
            } else {
                match rusqlite::Connection::open_with_flags(
                    &cdb,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                ) {
                    Ok(c) => {
                        let _ = c.busy_timeout(std::time::Duration::from_millis(1500));
                        match c.query_row("SELECT COUNT(*) FROM session", [], |r| r.get::<_, i64>(0)) {
                            Ok(n) => format!("{n} sessions"),
                            Err(e) => format!("count failed ({e})"),
                        }
                    }
                    Err(e) => format!("cannot open ({e})"),
                }
            };
            let line = format!("opencode db:   candidate {} — {}", cdb.display(), verdict);
            if summary.0 == 0 {
                crate::dlog::warn(|| line.clone());
            } else {
                crate::dlog::debug(|| line.clone());
            }
        }
    }
    let prefix = format!("{DB_PREFIX}/");
    let remote: std::collections::HashMap<&str, (&str, i64)> = listing
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(k, m)| (k.as_str(), (m.hash.as_str(), m.mtime_ms)))
        .collect();
    let mut store_set: Vec<String> = remote.iter().map(|(k, (h, _))| format!("{k}={h}")).collect();
    store_set.sort();
    let summary_hash = hash_bytes(format!("{summary:?}|{}", store_set.join(";")).as_bytes());
    const SUMMARY_KEY: &str = "opencode/db#local-summary";
    if state.files.get(SUMMARY_KEY).map(|s| s.hash == summary_hash).unwrap_or(false) {
        crate::dlog::debug(|| "opencode db: db and store unchanged since last export — skipping".to_string());
        return Ok(0);
    }
    let sessions = query_maps(&conn, "SELECT * FROM session", &[])?;
    let mut pushed = 0;
    let mut unchanged = 0;
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
        let local_eff = session
            .get("time_updated")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(max_time_of(messages.iter()))
            .max(max_time_of(parts.iter()));
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
        // Byte equality is too strict across machines: a merged session
        // re-serializes with per-machine differences (path separators,
        // schema drift), which made every machine "correct" the store each
        // sync, forever. The store copy's recorded version settles it: only
        // strictly newer local content is worth publishing. A legacy object
        // (mtime 0, pre-versioning) is re-put once to upgrade its metadata.
        // Versions are OpenCode's wall-clock stamps — with badly skewed
        // machine clocks, last-writer-wins can briefly favor the wrong copy,
        // the same trade OpenCode itself makes.
        if let Some((rhash, rmtime)) = remote.get(logical.as_str()) {
            if *rmtime > 0 && (*rhash == hash || *rmtime >= local_eff) {
                unchanged += 1;
                continue;
            }
        }
        crate::dlog::debug(|| {
            format!("opencode db: exporting {id} ({} messages, {} KB)", obj["messages"].as_array().map(|a| a.len()).unwrap_or(0), bytes.len() / 1024)
        });
        store.put(
            &logical,
            &bytes,
            &RemoteMeta { hash: hash.clone(), mtime_ms: local_eff, size: bytes.len() as u64, source: machine.to_string() },
        )?;
        state.files.insert(
            logical,
            FileState { hash, mtime_ms: local_eff, size: bytes.len() as u64, deleted_locally: false },
        );
        pushed += 1;
    }
    state.files.insert(
        SUMMARY_KEY.to_string(),
        FileState { hash: summary_hash, mtime_ms: 0, size: 0, deleted_locally: false },
    );
    if pushed > 0 {
        crate::dlog::info(|| format!("opencode db: exported {pushed} session(s), {unchanged} already in store"));
    } else {
        crate::dlog::debug(|| format!("opencode db: nothing to export ({unchanged} already in store)"));
    }
    Ok(pushed)
}

/// Merge foreign sessions into opencode.db: insert missing, update only when
/// the remote is newer (max timestamp across session+messages+parts; the
/// session row itself is replaced only if the remote ROW is newer), never
/// delete. The db is backed up once (`opencode.db.vibesync-bak`) before this
/// build's first-ever write.
pub fn db_apply(
    home: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let Some(db) = db_path(home) else {
        crate::dlog::warn(|| "opencode db: no local opencode.db — cannot import sessions".to_string());
        return Ok(report);
    };
    let prefix = format!("{DB_PREFIX}/");
    let mut seen = 0usize;
    let mut awaiting_folder = 0usize;
    let mut conn: Option<rusqlite::Connection> = None;
    for (logical, meta) in listing {
        if !logical.starts_with(&prefix) {
            continue;
        }
        seen += 1;
        on_file();
        if let Some(st) = state.files.get(logical) {
            // The mtime guard covers the listing snapshot predating our own
            // push this sync: we already hold a version at least as new, so
            // don't re-download what we just uploaded.
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
            let d = session.get("directory").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            crate::dlog::debug(|| format!("opencode db: {logical} parked — directory {d} has no mapping on this machine"));
            report.parked += 1; // project unknown here; retry when it appears
            continue;
        }
        let (Some(id), Some(remote_ses_t)) = (
            session.get("id").and_then(|v| v.as_str()).map(String::from),
            session.get("time_updated").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let ses_dir = session.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let remote_eff = remote_ses_t
            .max(max_time_of_json(obj.get("messages")))
            .max(max_time_of_json(obj.get("parts")));
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
        let local_ses_t: Option<i64> = c
            .query_row("SELECT time_updated FROM session WHERE id = ?1", [&id], |r| r.get(0))
            .ok();
        if let Some(local_t) = local_ses_t {
            let local_child: i64 = c
                .query_row(
                    "SELECT COALESCE((SELECT MAX(MAX(time_created, time_updated)) FROM message WHERE session_id = ?1), 0),
                            COALESCE((SELECT MAX(MAX(time_created, time_updated)) FROM part WHERE session_id = ?1), 0)",
                    [&id],
                    |r| Ok(std::cmp::max(r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
                .unwrap_or(0);
            if local_t.max(local_child) >= remote_eff {
                crate::dlog::debug(|| format!("opencode db: {id} kept — local copy is same or newer"));
                report.skipped_newer_local += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                );
                continue;
            }
        }
        crate::dlog::debug(|| format!("opencode db: merging session {id} into opencode.db"));
        // OpenCode's project ids are repo-derived and identical across
        // machines, but each machine clones the repo wherever it likes
        // (live-verified 2026-07-12: two Macs, one repo, two paths, same
        // id). When this project already exists here at a different
        // worktree, a root-anchored session is relocated to the local clone
        // so OpenCode lists it there.
        let mut relocate: Option<String> = None;
        if let Some(project) = obj.get_mut("project").and_then(|p| p.as_object_mut()) {
            expand_field(project, "worktree", tok);
            let pid = project.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let remote_worktree =
                project.get("worktree").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if pid != "global" && !remote_worktree.is_empty() && ses_dir == remote_worktree {
                if let Ok(lw) = c.query_row(
                    "SELECT worktree FROM project WHERE id = ?1",
                    [&pid],
                    |r| r.get::<_, String>(0),
                ) {
                    if lw != ses_dir && Path::new(&lw).is_dir() {
                        relocate = Some(lw);
                    }
                }
            }
            insert_map(c, "project", project, false)?; // never clobber a local project row
        }
        if let Some(lw) = &relocate {
            crate::dlog::debug(|| format!("opencode db: {id} relocated to local clone {lw}"));
            if let Some(sm) = obj.get_mut("session").and_then(|v| v.as_object_mut()) {
                sm.insert("directory".to_string(), serde_json::Value::String(lw.clone()));
            }
        }
        let session = obj.get("session").and_then(|s| s.as_object()).unwrap();
        // Replace the session ROW only when the remote row itself is newer —
        // a remote that wins on child-row activity alone must not revert
        // local session metadata (title, archive state, ...).
        if local_ses_t.map(|t| remote_ses_t > t).unwrap_or(true) {
            insert_map(c, "session", session, true)?;
        }
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
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        // OpenCode scopes global-project sessions to their exact directory:
        // a merged session stays invisible until that folder exists and is
        // opened. Say so, or "synced but not visible" reads as a bug.
        if let Some(d) = session.get("directory").and_then(|v| v.as_str()) {
            if !Path::new(d).exists() {
                awaiting_folder += 1;
                let d = d.to_string();
                crate::dlog::debug(|| {
                    format!("opencode db: {id} merged — appears in OpenCode once {d} exists and is opened")
                });
            }
        }
        on_pulled(logical);
        report.applied += 1;
    }
    crate::dlog::debug(|| {
        format!(
            "opencode db: store holds {seen} session export(s) — {} merged ({awaiting_folder} awaiting their folder), {} unchanged, {} kept (local newer), {} parked",
            report.applied, report.unchanged, report.skipped_newer_local, report.parked
        )
    });
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
                // An empty db can be a vestige on a machine whose OpenCode
                // uses the file layers — fall through and count those.
                if n > 0 {
                    return (n, bytes, p, last);
                }
            }
        }
    }
    // File layers: legacy storage/session/ plus each project/<slug>/storage/session/.
    let Some(base) = data_root(home) else { return (0, 0, 0, None) };
    let mut session_dirs = vec![base.join("storage").join("session")];
    let mut project_slugs = 0usize;
    if let Ok(rd) = std::fs::read_dir(base.join("project")) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                project_slugs += 1;
                session_dirs.push(e.path().join("storage").join("session"));
            }
        }
    }
    let root = base.join("storage");
    let mut sessions = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    for dir in session_dirs {
        for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                sessions += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if let Ok(m) = mtime_ms(p) {
                    last = Some(last.map_or(m, |l: i64| l.max(m)));
                }
            }
        }
    }
    let legacy_projects = walkdir::WalkDir::new(root.join("project"))
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count();
    (sessions, bytes, legacy_projects + project_slugs, last)
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
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 1);
        // Re-push with no change: the export is in the listing — skipped.
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &store.list().unwrap()).unwrap(), 0);
        // A store that lost the export gets it again even though local state
        // says "already pushed" — the listing, not state, is authoritative.
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &[]).unwrap(), 1);

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
        // Ping-pong guard: B re-serializes the merged session with different
        // bytes (here: a title tweak with time_updated untouched). Equal
        // time_updated means the store copy is authoritative — no push.
        c.execute("UPDATE session SET title = 'renamed locally' WHERE id = 'ses1'", [])
            .unwrap();
        assert_eq!(db_push(&b, &tok_b, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(), 0);
        let msgs: i64 = c.query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0)).unwrap();
        let parts: i64 = c.query_row("SELECT COUNT(*) FROM part", [], |r| r.get(0)).unwrap();
        assert_eq!((msgs, parts), (1, 1));
        // Backup was taken before the first write.
        assert!(db_b.with_extension("db.vibesync-bak").exists());

        // Push direction: strictly newer local content must overwrite the
        // byte-divergent store copy...
        c.execute("UPDATE session SET title = 'resumed on b', time_updated = 300 WHERE id = 'ses1'", [])
            .unwrap();
        assert_eq!(db_push(&b, &tok_b, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(), 1);
        // ...including a message written WITHOUT a session-row bump (the
        // export version is the max timestamp across session+messages+parts).
        c.execute("INSERT INTO message VALUES ('msg2','ses1',400,400,'{\"role\":\"assistant\"}')", [])
            .unwrap();
        assert_eq!(db_push(&b, &tok_b, &mut st_b, &store, "b", &store.list().unwrap()).unwrap(), 1);
        // A merges the late message; its own session row (older, t=200) is
        // replaced by B's newer row, and the new message lands.
        let r = db_apply(&a, &tok_a, &mut st_a, &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap();
        assert_eq!(r.applied, 1);
        let ca = rusqlite::Connection::open(&db_a).unwrap();
        let (title_a, n_msgs): (String, i64) = ca
            .query_row(
                "SELECT title, (SELECT COUNT(*) FROM message WHERE session_id='ses1') FROM session WHERE id='ses1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((title_a.as_str(), n_msgs), ("resumed on b", 2));

        // Local session newer than remote: untouched.
        c.execute("UPDATE session SET title='local edit', time_updated=999 WHERE id='ses1'", [])
            .unwrap();
        let mut st_b2 = SyncState::default();
        let r = db_apply(&b, &tok_b, &mut st_b2, &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap();
        assert_eq!(r.skipped_newer_local, 1);
        let title: String =
            c.query_row("SELECT title FROM session WHERE id='ses1'", [], |r| r.get(0)).unwrap();
        assert_eq!(title, "local edit");
    }

    #[test]
    fn session_relocates_to_local_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let db_a = make_db(&a);
        std::fs::write(a.join(".local/share/opencode/auth.json"), "{}").unwrap();
        {
            let c = rusqlite::Connection::open(&db_a).unwrap();
            c.execute_batch(
                "INSERT INTO project VALUES ('prj9', '/Users/u/proj/app', 1, 1, '[]');
                 INSERT INTO session VALUES ('ses9','prj9','t','/Users/u/proj/app','Repo session','1.0',100,200);
                 INSERT INTO message VALUES ('m9','ses9',100,100,'{}');
                 INSERT INTO part VALUES ('p9','m9','ses9',100,100,'{}');",
            )
            .unwrap();
        }
        let tok_a = Tokenizer::with_case_sensitivity("/Users/u", false);
        let mut st_a = SyncState::default();
        assert_eq!(db_push(&a, &tok_a, &mut st_a, &store, "a", &[]).unwrap(), 1);

        // B cloned the same repo somewhere else: OpenCode's repo-derived
        // project id matches, the path does not.
        let b = tmp.path().join("b");
        let db_b = make_db(&b);
        std::fs::write(b.join(".local/share/opencode/auth.json"), "{}").unwrap();
        let clone = tmp.path().join("bobclone");
        std::fs::create_dir_all(&clone).unwrap();
        let clone_s = clone.to_string_lossy().into_owned();
        rusqlite::Connection::open(&db_b)
            .unwrap()
            .execute("INSERT INTO project VALUES ('prj9', ?1, 1, 1, '[]')", [&clone_s])
            .unwrap();
        let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
        let mut st_b = SyncState::default();
        let r = db_apply(&b, &tok_b, &mut st_b, &store, &store.list().unwrap(), &|| {}, &|_| {})
            .unwrap();
        assert_eq!(r.applied, 1);
        let c = rusqlite::Connection::open(&db_b).unwrap();
        let dir: String =
            c.query_row("SELECT directory FROM session WHERE id='ses9'", [], |r| r.get(0)).unwrap();
        assert_eq!(dir, clone_s);
        // The local project row keeps its own worktree.
        let wt: String =
            c.query_row("SELECT worktree FROM project WHERE id='prj9'", [], |r| r.get(0)).unwrap();
        assert_eq!(wt, clone_s);
    }

    #[test]
    fn project_layout_syncs_additively() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let g = a.join(".local/share/opencode/project/global/storage/session");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("ses_9.json"), "{\"id\":\"ses_9\"}").unwrap();
        let m = a.join(".local/share/opencode/project/my-repo/storage/message/ses_8");
        std::fs::create_dir_all(&m).unwrap();
        std::fs::write(m.join("msg_1.json"), "{\"role\":\"user\"}").unwrap();
        // Volatile dirs are skipped inside project storage too.
        let sd = a.join(".local/share/opencode/project/global/storage/session_diff");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("d.json"), "x").unwrap();

        let entries = scan(&a).unwrap();
        let logicals: Vec<&str> = entries.iter().map(|e| e.logical.as_str()).collect();
        assert_eq!(
            logicals,
            [
                "opencode/project/global/storage/session/ses_9.json",
                "opencode/project/my-repo/storage/message/ses_8/msg_1.json",
            ]
        );
        let mut st_a = SyncState::default();
        crate::sync::push(&entries, &mut st_a, &store, "a").unwrap();

        let b = tmp.path().join("b");
        std::fs::create_dir_all(b.join(".local/share/opencode")).unwrap();
        let mut st_b = SyncState::default();
        let listing = store.list().unwrap();
        let r = apply(&b, &mut st_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(r.applied, 2);
        assert!(b.join(".local/share/opencode/project/global/storage/session/ses_9.json").exists());
        assert!(b
            .join(".local/share/opencode/project/my-repo/storage/message/ses_8/msg_1.json")
            .exists());
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
        assert_eq!(report.applied, 1);
        assert!(b.join(".local/share/opencode/storage/session/ses_1.json").exists());
    }
}
