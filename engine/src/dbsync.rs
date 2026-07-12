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

pub(crate) fn expand_field(
    m: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    tok: &Tokenizer,
) {
    if let Some(serde_json::Value::String(s)) = m.get(key) {
        let e = normalize_path_shape(&tok.expand_plain(s));
        m.insert(key.to_string(), serde_json::Value::String(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
