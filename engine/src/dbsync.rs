//! Shared machinery for db-level session sync (OpenCode, Codex, ...).
//!
//! Rows travel as generic column maps so schema drift between tool versions
//! degrades gracefully: unknown columns are dropped on insert, missing ones
//! hit the local table's defaults. Timestamps version each export; the
//! store copy's recorded version — not byte equality — decides pushes
//! (re-serialization drifts across machines and must not ping-pong).
//!
//! Writes never delete: a row is updated IN PLACE or inserted when absent
//! (see [`insert_map_pk`]), each merged item is one transaction, and the
//! first write to a tool's db is preceded by a consistent backup
//! ([`open_for_merge`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::types::Value as SqlValue;

use crate::tokenizer::Tokenizer;

pub(crate) fn row_to_map(
    row: &rusqlite::Row,
    cols: &[String],
) -> serde_json::Map<String, serde_json::Value> {
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

pub(crate) fn query_maps(
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

/// Tool db timestamps are often ISO-8601 TEXT (`2026-07-13T12:41:13.292Z`, or
/// SQLite's `2026-07-13 12:41:13` default form) — parse to epoch ms so
/// they can version store objects. Unparseable → 0 (never blocks a merge).
pub(crate) fn iso_ms(s: &str) -> i64 {
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

/// True when `table` exists. Tool generations add and drop tables; one this
/// machine's version lacks must be skipped, never fail the whole sync.
pub(crate) fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// (name, declared type) of every column of a local table — empty when the
/// table doesn't exist.
fn table_info(conn: &rusqlite::Connection, table: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let cols = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default()))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Columns of a local table — inserts are filtered to these so an object
/// from a newer tool version can't fail the whole apply.
pub(crate) fn table_cols(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    Ok(table_info(conn, table)?.into_iter().map(|(n, _)| n).collect())
}

pub(crate) fn has_column(conn: &rusqlite::Connection, table: &str, col: &str) -> bool {
    table_cols(conn, table).map(|cs| cs.iter().any(|c| c == col)).unwrap_or(false)
}

/// Every row of `table` whose `fk` column equals `id`, for an export.
/// Schema drift yields fewer rows instead of a failed export: a table (or
/// link column) this tool version lacks gives none, and ordering uses
/// whichever `order` columns exist (ordering keeps exports byte-stable).
pub(crate) fn child_rows(
    conn: &rusqlite::Connection,
    table: &str,
    fk: &str,
    id: &str,
    order: &[&str],
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let cols = table_cols(conn, table)?;
    if !cols.iter().any(|c| c == fk) {
        return Ok(Vec::new());
    }
    let order: Vec<String> =
        order.iter().filter(|o| cols.iter().any(|c| c == *o)).map(|o| format!("`{o}`")).collect();
    let order = if order.is_empty() { String::new() } else { format!(" ORDER BY {}", order.join(",")) };
    query_maps(conn, &format!("SELECT * FROM `{table}` WHERE `{fk}` = ?1{order}"), &[&id])
}

/// Strict hex decode (exports carry BLOBs as lowercase hex): `None` for an
/// odd length or any non-hex byte. Never panics on multi-byte text and
/// never invents 0x00 bytes for garbage.
pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = b.as_chunks::<2>();
    pairs
        .iter()
        .map(|[hi, lo]| Some(((*hi as char).to_digit(16)? * 16 + (*lo as char).to_digit(16)?) as u8))
        .collect()
}

fn is_blob_decl(decl: &str) -> bool {
    decl.to_ascii_uppercase().contains("BLOB")
}

/// JSON value -> SQLite value. Exports carry BLOBs as hex strings — the
/// wire format older builds read and write, so it can't change — but for a
/// column the LOCAL table declares BLOB, a valid hex string goes back in as
/// the real bytes instead of as TEXT. Anything else stays text.
fn sql_value(v: &serde_json::Value, blob_col: bool) -> SqlValue {
    match v {
        serde_json::Value::Null => SqlValue::Null,
        serde_json::Value::Bool(b) => SqlValue::Integer(*b as i64),
        serde_json::Value::Number(n) if n.is_i64() => SqlValue::Integer(n.as_i64().unwrap()),
        serde_json::Value::Number(n) => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) if blob_col => match hex_decode(s) {
            Some(bytes) => SqlValue::Blob(bytes),
            None => SqlValue::Text(s.clone()),
        },
        serde_json::Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// `WHERE` condition + params selecting the row `map` describes by its
/// `key` columns; `None` when the map lacks one of them.
fn key_filter(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &[&str],
) -> Option<(String, Vec<SqlValue>)> {
    if key.is_empty() || !key.iter().all(|k| map.contains_key(*k)) {
        return None;
    }
    let cond = key.iter().map(|k| format!("`{k}` = ?")).collect::<Vec<_>>().join(" AND ");
    Some((cond, key.iter().map(|k| sql_value(&map[*k], false)).collect()))
}

/// What a row write did — callers tally these for the debug trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowWrite {
    Inserted,
    /// The remote copy won and replaced the local columns it carries.
    Updated,
    /// Local copy kept; only columns it had no value for (NULL) were filled.
    Filled,
    /// Already identical.
    Unchanged,
    /// Colliding row with different content; the local copy stays.
    KeptLocal,
    /// This tool version has no such table — skipped.
    NoTable,
}

/// Running counts of [`RowWrite`]s for one merged item.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RowTally {
    pub added: usize,
    pub updated: usize,
    pub kept: usize,
}

impl RowTally {
    pub(crate) fn add(&mut self, w: RowWrite) {
        match w {
            RowWrite::Inserted => self.added += 1,
            RowWrite::Updated | RowWrite::Filled => self.updated += 1,
            RowWrite::KeptLocal => self.kept += 1,
            RowWrite::Unchanged | RowWrite::NoTable => {}
        }
    }
}

fn insert_row(
    conn: &rusqlite::Connection,
    table: &str,
    info: &[(String, String)],
    map: &serde_json::Map<String, serde_json::Value>,
    verb: &str,
) -> Result<usize> {
    let cols: Vec<&(String, String)> = info.iter().filter(|(c, _)| map.contains_key(c)).collect();
    if cols.is_empty() {
        return Ok(0);
    }
    let names = cols.iter().map(|(c, _)| format!("`{c}`")).collect::<Vec<_>>().join(",");
    let placeholders = vec!["?"; cols.len()].join(",");
    let params: Vec<SqlValue> = cols.iter().map(|(c, t)| sql_value(&map[c], is_blob_decl(t))).collect();
    let sql = format!("{verb} INTO `{table}` ({names}) VALUES ({placeholders})");
    Ok(conn.execute(&sql, rusqlite::params_from_iter(params))?)
}

/// UPDATE only the `set` columns of the row matching `cond` — every column
/// the incoming map doesn't carry keeps its local value AND storage type
/// (re-writing untouched columns turned local BLOBs into hex TEXT).
fn update_row(
    conn: &rusqlite::Connection,
    table: &str,
    info: &[(String, String)],
    map: &serde_json::Map<String, serde_json::Value>,
    set: &[&str],
    cond: &str,
    key_params: Vec<SqlValue>,
) -> Result<usize> {
    let blob = |c: &str| info.iter().any(|(n, t)| n == c && is_blob_decl(t));
    let assigns = set.iter().map(|c| format!("`{c}` = ?")).collect::<Vec<_>>().join(", ");
    let mut params: Vec<SqlValue> = set.iter().map(|c| sql_value(&map[*c], blob(c))).collect();
    params.extend(key_params);
    let sql = format!("UPDATE `{table}` SET {assigns} WHERE {cond}");
    Ok(conn.execute(&sql, rusqlite::params_from_iter(params))?)
}

/// Write one row. `or_replace`: an existing row (found by `pk`) is UPDATED
/// IN PLACE with the incoming columns, otherwise the row is inserted.
/// Without `or_replace` an existing row is never touched.
///
/// Never `INSERT OR REPLACE`: REPLACE deletes the old row before inserting,
/// and the bundled SQLite enforces foreign keys — replacing an OpenCode
/// session row fired `ON DELETE CASCADE` and wiped every local message,
/// part and todo of that session. Updating in place also keeps columns a
/// sender on an older schema doesn't know about (a wiped `preview` hides a
/// thread from Codex's list entirely). A table this tool version lacks is
/// skipped (`NoTable`), not an error.
pub(crate) fn insert_map_pk(
    conn: &rusqlite::Connection,
    table: &str,
    map: &serde_json::Map<String, serde_json::Value>,
    or_replace: bool,
    pk: &[&str],
) -> Result<RowWrite> {
    let info = table_info(conn, table)?;
    if info.is_empty() {
        return Ok(RowWrite::NoTable);
    }
    if !or_replace {
        let n = insert_row(conn, table, &info, map, "INSERT OR IGNORE")?;
        return Ok(if n > 0 { RowWrite::Inserted } else { RowWrite::Unchanged });
    }
    if let Some((cond, key_params)) = key_filter(map, pk) {
        let set: Vec<&str> = info
            .iter()
            .map(|(c, _)| c.as_str())
            .filter(|c| map.contains_key(*c) && !pk.contains(c))
            .collect();
        let found = if set.is_empty() {
            conn.query_row(
                &format!("SELECT COUNT(*) FROM `{table}` WHERE {cond}"),
                rusqlite::params_from_iter(key_params),
                |r| r.get::<_, i64>(0),
            )? as usize
        } else {
            update_row(conn, table, &info, map, &set, &cond, key_params)?
        };
        if found > 0 {
            return Ok(RowWrite::Updated);
        }
    }
    // A plain INSERT: a clash on some other constraint fails this item
    // (rolled back, retried next sync) — never a silent delete.
    let n = insert_row(conn, table, &info, map, "INSERT")?;
    Ok(if n > 0 { RowWrite::Inserted } else { RowWrite::Unchanged })
}

/// How a child row that exists on both sides with different content picks
/// a winner.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RowVersion<'a> {
    /// The row's own timestamp column(s) (epoch ms or ISO text): a strictly
    /// newer remote copy wins; equal or unknown keeps the local copy.
    Columns(&'a [&'a str]),
    /// The row has no version of its own: the caller's verdict decides
    /// (e.g. "the remote PARENT row is strictly newer"); `false` keeps the
    /// local copy.
    RemoteNewer(bool),
}

/// Newest of `cols` in a row map, as epoch ms (0 when none parse).
fn row_time(m: &serde_json::Map<String, serde_json::Value>, cols: &[&str]) -> i64 {
    cols.iter()
        .filter_map(|c| match m.get(*c)? {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => Some(iso_ms(s)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Merge one child row (message, part, turn, ...) identified by `key`:
/// - absent locally -> inserted;
/// - identical -> untouched;
/// - colliding with different content -> the remote copy replaces the
///   local columns only when `version` says it is strictly newer. Otherwise
///   the local copy stays — two machines that continued a session from the
///   same point hold DIFFERENT turns under the same index, and neither may
///   overwrite the other — except that columns the local copy has no value
///   for yet (NULL: a turn synced mid-reply) are filled in, losing nothing.
///
/// Never deletes; a table this tool version lacks is skipped.
pub(crate) fn merge_row(
    conn: &rusqlite::Connection,
    table: &str,
    remote: &serde_json::Map<String, serde_json::Value>,
    key: &[&str],
    version: RowVersion,
) -> Result<RowWrite> {
    let info = table_info(conn, table)?;
    if info.is_empty() {
        return Ok(RowWrite::NoTable);
    }
    let Some((cond, key_params)) = key_filter(remote, key) else {
        // No identity to match on: it can only be added, never replace.
        let n = insert_row(conn, table, &info, remote, "INSERT OR IGNORE")?;
        return Ok(if n > 0 { RowWrite::Inserted } else { RowWrite::Unchanged });
    };
    let refs: Vec<&dyn rusqlite::ToSql> =
        key_params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let local = query_maps(conn, &format!("SELECT * FROM `{table}` WHERE {cond}"), &refs)?.pop();
    let Some(local) = local else {
        let n = insert_row(conn, table, &info, remote, "INSERT")?;
        return Ok(if n > 0 { RowWrite::Inserted } else { RowWrite::Unchanged });
    };
    let differing: Vec<&str> = info
        .iter()
        .map(|(c, _)| c.as_str())
        .filter(|c| !key.contains(c))
        .filter(|c| remote.get(*c).is_some_and(|rv| local.get(*c) != Some(rv)))
        .collect();
    if differing.is_empty() {
        return Ok(RowWrite::Unchanged);
    }
    let remote_wins = match version {
        RowVersion::Columns(cols) => row_time(remote, cols) > row_time(&local, cols),
        RowVersion::RemoteNewer(newer) => newer,
    };
    let set: Vec<&str> = if remote_wins {
        differing
    } else {
        differing.into_iter().filter(|c| local.get(*c).is_none_or(|v| v.is_null())).collect()
    };
    if set.is_empty() {
        return Ok(RowWrite::KeptLocal);
    }
    update_row(conn, table, &info, remote, &set, &cond, key_params)?;
    Ok(if remote_wins { RowWrite::Updated } else { RowWrite::Filled })
}

/// `<file name><suffix>` beside `p` (`opencode.db` -> `opencode.db.vibesync-bak`).
fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

/// Open a tool's db for writing. The busy timeout rides out the tool's own
/// brief write locks instead of failing on the first one.
pub(crate) fn open_rw(db: &Path) -> Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open(db)?;
    c.busy_timeout(std::time::Duration::from_millis(1500))?;
    Ok(c)
}

/// The one-time safety copy taken before VibeSync's first-ever write to a
/// tool's db: `<db>.vibesync-bak`, kept as is once it exists.
///
/// These dbs run in WAL mode — recent commits live in `-wal` until a
/// checkpoint, so a plain copy of the main file misses them and can be torn
/// by a concurrent writer. `VACUUM INTO` writes a consistent snapshot that
/// includes the WAL. It lands under a temp name first, so an interrupted
/// backup never leaves a truncated file that later passes for the real one.
/// An error means NO write may follow: callers abort the merge for this sync.
pub(crate) fn backup_db_once(conn: &rusqlite::Connection, db: &Path) -> Result<PathBuf> {
    let bak = sibling(db, ".vibesync-bak");
    if bak.exists() {
        return Ok(bak);
    }
    let tmp = sibling(db, ".vibesync-bak.tmp");
    if tmp.is_file() {
        std::fs::remove_file(&tmp)?; // residue of an interrupted backup
    }
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])
        .with_context(|| format!("snapshot {} into {}", db.display(), tmp.display()))?;
    std::fs::rename(&tmp, &bak)?;
    crate::dlog::info(|| format!("backed up {} to {} before the first merge", db.display(), bak.display()));
    Ok(bak)
}

/// [`open_rw`] + [`backup_db_once`]: the way merge code gets a writable
/// connection, so no write can precede the backup.
pub(crate) fn open_for_merge(db: &Path) -> Result<rusqlite::Connection> {
    let c = open_rw(db)?;
    backup_db_once(&c, db).with_context(|| {
        format!("cannot back up {} before writing to it — nothing merged this sync", db.display())
    })?;
    Ok(c)
}

pub(crate) fn tokenize_field(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    tok: &Tokenizer,
) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let t = tok.tokenize_plain(s);
        m.insert(key.to_string(), serde_json::Value::String(t));
    }
}

/// A `${HOME}`-tokenized path keeps the ORIGIN machine's separators in its
/// tail, so cross-OS expansion yields `/Users/x\.codex\...` — which then
/// fails every exists() check even though the file is right there. Make the
/// expanded value self-consistent with its own root shape.
pub(crate) fn normalize_path_shape(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s.replace('/', "\\")
    } else if s.starts_with('/') {
        s.replace('\\', "/")
    } else {
        s.to_string()
    }
}

/// True when a path clearly belongs to the OTHER OS family than `home` —
/// a Windows drive path on a unix machine or a POSIX-absolute path on a
/// Windows one. Such values come from pre-tokenization exports or from a
/// tool's own backfill and need adoption or healing.
pub(crate) fn foreign_shaped(s: &str, home: &str) -> bool {
    let s = s.strip_prefix("\\\\?\\").unwrap_or(s);
    let windows_home = home.as_bytes().get(1) == Some(&b':');
    let windows_path = s.len() >= 2
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.as_bytes()[1] == b':';
    if windows_home {
        s.starts_with('/')
    } else {
        windows_path
    }
}

/// Map a path under ANOTHER OS's user-home onto the local home — the same
/// meaning `${HOME}` tokenization would have carried, recovered after the
/// fact (`\\?\C:\Users\bob\Documents\X` -> `/Users/alice/Documents/X`).
/// Paths outside a recognizable home (C:\Temp\...) don't translate: None.
pub(crate) fn adopt_foreign_home(s: &str, home: &str) -> Option<String> {
    if !foreign_shaped(s, home) {
        return None;
    }
    let s = s.strip_prefix("\\\\?\\").unwrap_or(s);
    let norm = s.replace('\\', "/");
    let rest = if norm.as_bytes().get(1) == Some(&b':') {
        // X:/Users/<name>/rest
        let mut parts = norm.splitn(4, '/');
        let (_drive, users, _name, rest) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if !users.eq_ignore_ascii_case("Users") {
            return None;
        }
        rest.to_string()
    } else {
        // /Users/<name>/rest or /home/<name>/rest
        let mut parts = norm.splitn(4, '/');
        let (_e, base, _name, rest) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if base != "Users" && base != "home" {
            return None;
        }
        rest.to_string()
    };
    Some(normalize_path_shape(&format!("{}/{rest}", home.trim_end_matches(['/', '\\']))))
}

/// Translate one stored path onto this machine: token expansion, the
/// path's own separator shape, foreign homes adopted.
pub(crate) fn expand_path(s: &str, tok: &Tokenizer) -> String {
    let e = normalize_path_shape(&tok.expand_plain(s));
    adopt_foreign_home(&e, tok.home()).unwrap_or(e)
}

pub(crate) fn expand_field(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    tok: &Tokenizer,
) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let e = expand_path(s, tok);
        m.insert(key.to_string(), serde_json::Value::String(e));
    }
}

/// The max time_created/time_updated across a set of rows — a session's
/// export version must cover child rows too (a late message must travel
/// even when the parent row's timestamp didn't move).
pub(crate) fn max_time_of<'a>(
    rows: impl Iterator<Item = &'a serde_json::Map<String, serde_json::Value>>,
) -> i64 {
    rows.flat_map(|r| {
        ["time_created", "time_updated"]
            .into_iter()
            .filter_map(|k| r.get(k).and_then(|v| v.as_i64()))
    })
    .max()
    .unwrap_or(0)
}

pub(crate) fn max_time_of_json(rows: Option<&serde_json::Value>) -> i64 {
    max_time_of(rows.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_object()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn replace_overlays_instead_of_wiping() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL,
               preview TEXT NOT NULL DEFAULT '');
             INSERT INTO threads VALUES ('t1', 'old title', 'the preview');",
        )
        .unwrap();
        // A sender on an older schema knows nothing about `preview`.
        let mut incoming = serde_json::Map::new();
        incoming.insert("id".into(), serde_json::Value::String("t1".into()));
        incoming.insert("title".into(), serde_json::Value::String("new title".into()));
        insert_map_pk(&conn, "threads", &incoming, true, &["id"]).unwrap();
        let (title, preview): (String, String) = conn
            .query_row("SELECT title, preview FROM threads WHERE id='t1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(title, "new title");
        assert_eq!(preview, "the preview"); // NOT wiped to the default
    }

    #[test]
    fn replacing_a_parent_row_never_cascades() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE);
             INSERT INTO session VALUES ('s', 'old');
             INSERT INTO message VALUES ('m', 's');",
        )
        .unwrap();
        let w = insert_map_pk(&conn, "session", &obj(serde_json::json!({"id": "s", "title": "new"})), true, &["id"])
            .unwrap();
        assert_eq!(w, RowWrite::Updated);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "the child row must survive its parent's update");
    }

    #[test]
    fn missing_tables_are_skipped_not_fatal() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let row = obj(serde_json::json!({"id": "x"}));
        assert_eq!(insert_map_pk(&conn, "nope", &row, true, &["id"]).unwrap(), RowWrite::NoTable);
        assert_eq!(
            merge_row(&conn, "nope", &row, &["id"], RowVersion::RemoteNewer(true)).unwrap(),
            RowWrite::NoTable
        );
        assert!(child_rows(&conn, "nope", "id", "x", &["id"]).unwrap().is_empty());
        assert!(!table_exists(&conn, "nope"));
    }

    #[test]
    fn merge_row_never_overwrites_a_newer_or_undatable_local_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE part (id TEXT PRIMARY KEY, time_updated INTEGER, data TEXT, note TEXT);
             INSERT INTO part VALUES ('p', 500, 'local', NULL);",
        )
        .unwrap();
        let data = |c: &rusqlite::Connection| -> (String, Option<String>) {
            c.query_row("SELECT data, note FROM part WHERE id='p'", [], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
        };
        let v = RowVersion::Columns(&["time_updated"]);
        // Older remote: local stays — but its empty column is filled in.
        let older = obj(serde_json::json!({"id": "p", "time_updated": 100, "data": "remote", "note": "n"}));
        assert_eq!(merge_row(&conn, "part", &older, &["id"], v).unwrap(), RowWrite::Filled);
        assert_eq!(data(&conn), ("local".into(), Some("n".into())));
        // Same version, different content: can't tell — local stays.
        let tie = obj(serde_json::json!({"id": "p", "time_updated": 500, "data": "remote", "note": "n"}));
        assert_eq!(merge_row(&conn, "part", &tie, &["id"], v).unwrap(), RowWrite::KeptLocal);
        // No version at all: local stays.
        assert_eq!(
            merge_row(&conn, "part", &tie, &["id"], RowVersion::RemoteNewer(false)).unwrap(),
            RowWrite::KeptLocal
        );
        // Strictly newer remote: wins.
        let newer = obj(serde_json::json!({"id": "p", "time_updated": 900, "data": "remote", "note": "n"}));
        assert_eq!(merge_row(&conn, "part", &newer, &["id"], v).unwrap(), RowWrite::Updated);
        assert_eq!(data(&conn).0, "remote");
        // Absent: inserted.
        let fresh = obj(serde_json::json!({"id": "q", "time_updated": 1, "data": "new"}));
        assert_eq!(merge_row(&conn, "part", &fresh, &["id"], v).unwrap(), RowWrite::Inserted);
    }

    #[test]
    fn blobs_stay_blobs_through_merge() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id TEXT PRIMARY KEY, payload BLOB, local_only BLOB, label TEXT);
             INSERT INTO t VALUES ('a', x'00ff', x'cafe', 'old');",
        )
        .unwrap();
        let types = |c: &rusqlite::Connection, id: &str| -> (String, String, Vec<u8>) {
            c.query_row(
                "SELECT typeof(payload), typeof(local_only), payload FROM t WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        // An export (hex for blobs) updates the row: the incoming blob lands
        // as bytes, and the column the sender didn't carry keeps its blob.
        let upd = obj(serde_json::json!({"id": "a", "payload": "28b5", "label": "new"}));
        insert_map_pk(&conn, "t", &upd, true, &["id"]).unwrap();
        assert_eq!(types(&conn, "a"), ("blob".into(), "blob".into(), vec![0x28, 0xb5]));
        // Inserts too.
        let ins = obj(serde_json::json!({"id": "b", "payload": "0102", "label": "x"}));
        merge_row(&conn, "t", &ins, &["id"], RowVersion::RemoteNewer(false)).unwrap();
        assert_eq!(types(&conn, "b").2, vec![1, 2]);
        // Text in a TEXT column is never "decoded".
        let txt = obj(serde_json::json!({"id": "c", "label": "beef"}));
        insert_map_pk(&conn, "t", &txt, true, &["id"]).unwrap();
        let label: String = conn.query_row("SELECT label FROM t WHERE id='c'", [], |r| r.get(0)).unwrap();
        assert_eq!(label, "beef");
    }

    #[test]
    fn hex_decode_is_strict() {
        assert_eq!(hex_decode("28b52ffd"), Some(vec![0x28, 0xb5, 0x2f, 0xfd]));
        assert_eq!(hex_decode(""), Some(vec![]));
        assert_eq!(hex_decode("abc"), None, "odd length");
        assert_eq!(hex_decode("zz"), None, "not hex");
        assert_eq!(hex_decode("é0"), None, "multi-byte input must not panic");
    }

    #[test]
    fn backup_is_a_consistent_snapshot_and_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("tool.db");
        let live = rusqlite::Connection::open(&db).unwrap();
        live.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);",
        )
        .unwrap();
        let c = open_rw(&db).unwrap();
        let bak = backup_db_once(&c, &db).unwrap();
        assert_eq!(bak, tmp.path().join("tool.db.vibesync-bak"));
        let n: i64 = rusqlite::Connection::open(&bak)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "rows still in the WAL are in the backup");
        // Once taken, later writes never refresh it.
        live.execute("INSERT INTO t VALUES (2)", []).unwrap();
        backup_db_once(&c, &db).unwrap();
        let n: i64 = rusqlite::Connection::open(&bak)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn foreign_home_paths_are_adopted() {
        // Raw Windows cwd (extended-length, never tokenized) on a Mac.
        assert_eq!(
            adopt_foreign_home("\\\\?\\C:\\Users\\you\\Documents\\X", "/Users/alice").as_deref(),
            Some("/Users/alice/Documents/X")
        );
        // Outside any home: no adoption.
        assert_eq!(adopt_foreign_home("C:\\Temp\\X", "/Users/alice"), None);
        // Reverse direction: POSIX home path on a Windows machine.
        assert_eq!(
            adopt_foreign_home("/Users/alice/Documents/X", "C:\\Users\\you").as_deref(),
            Some("C:\\Users\\you\\Documents\\X")
        );
        // Same-family paths are left alone.
        assert_eq!(adopt_foreign_home("/Users/alice/x", "/Users/bob"), None);
    }

    #[test]
    fn cross_os_expansion_normalizes_separators() {
        let tok = Tokenizer::with_case_sensitivity("/Users/u", false);
        let mut m = serde_json::Map::new();
        // A Windows machine tokenized its rollout path with backslash tails.
        m.insert(
            "rollout_path".to_string(),
            serde_json::Value::String("${HOME}\\.codex\\sessions\\r.jsonl".to_string()),
        );
        expand_field(&mut m, "rollout_path", &tok);
        assert_eq!(m["rollout_path"], "/Users/u/.codex/sessions/r.jsonl");
        // And the reverse shape for drive-letter paths.
        assert_eq!(normalize_path_shape("C:\\Users\\w/dev/proj"), "C:\\Users\\w\\dev\\proj");
    }
}
