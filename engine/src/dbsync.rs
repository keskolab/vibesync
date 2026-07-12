//! Shared machinery for db-level session sync (OpenCode, Codex, ...).
//!
//! Rows travel as generic column maps so schema drift between tool versions
//! degrades gracefully: unknown columns are dropped on insert, missing ones
//! hit the local table's defaults. Timestamps version each export; the
//! store copy's recorded version — not byte equality — decides pushes
//! (re-serialization drifts across machines and must not ping-pong).

use anyhow::Result;

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

/// Columns of a local table — inserts are filtered to these so an object
/// from a newer tool version can't fail the whole apply.
fn table_cols(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

pub(crate) fn insert_map(
    conn: &rusqlite::Connection,
    table: &str,
    map: &serde_json::Map<String, serde_json::Value>,
    or_replace: bool,
) -> Result<()> {
    insert_map_pk(conn, table, map, or_replace, &[])
}

/// Like [`insert_map`], but when replacing an EXISTING row (looked up by
/// `pk` columns) the incoming map is OVERLAID on the current row first —
/// a sender on an older tool schema must never wipe columns it doesn't
/// know about back to their defaults (a wiped `preview` hides the thread
/// from Codex's list entirely).
pub(crate) fn insert_map_pk(
    conn: &rusqlite::Connection,
    table: &str,
    map: &serde_json::Map<String, serde_json::Value>,
    or_replace: bool,
    pk: &[&str],
) -> Result<()> {
    let mut merged;
    let map = if or_replace && !pk.is_empty() && pk.iter().all(|k| map.contains_key(*k)) {
        let cond = pk
            .iter()
            .enumerate()
            .map(|(i, k)| format!("`{k}` = ?{}", i + 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        let params: Vec<Box<dyn rusqlite::ToSql>> = pk
            .iter()
            .map(|k| -> Box<dyn rusqlite::ToSql> {
                match &map[*k] {
                    serde_json::Value::Number(n) if n.is_i64() => Box::new(n.as_i64().unwrap()),
                    serde_json::Value::String(v) => Box::new(v.clone()),
                    other => Box::new(other.to_string()),
                }
            })
            .collect();
        let existing = query_maps(
            conn,
            &format!("SELECT * FROM `{table}` WHERE {cond}"),
            &params.iter().map(|b| b.as_ref()).collect::<Vec<_>>(),
        )?
        .pop();
        match existing {
            Some(mut base) => {
                for (k, v) in map {
                    base.insert(k.clone(), v.clone());
                }
                merged = base;
                &merged
            }
            None => map,
        }
    } else {
        map
    };
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
    let windows_path = s.as_bytes().len() >= 2
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

pub(crate) fn expand_field(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    tok: &Tokenizer,
) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let mut e = normalize_path_shape(&tok.expand_plain(s));
        if let Some(adopted) = adopt_foreign_home(&e, tok.home()) {
            e = adopted;
        }
        m.insert(key.to_string(), serde_json::Value::String(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
