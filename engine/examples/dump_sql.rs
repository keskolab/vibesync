//! Debug helper: run a read-only query against any SQLite db and print
//! rows as JSON lines. Usage: dump_sql <db> <sql>
fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("usage: dump_sql <db> <sql>");
    let sql = args.next().expect("usage: dump_sql <db> <sql>");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut stmt = conn.prepare(&sql).unwrap();
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let mut obj = serde_json::Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v: rusqlite::types::Value = row.get(i).unwrap();
            let j = match v {
                rusqlite::types::Value::Null => serde_json::Value::Null,
                rusqlite::types::Value::Integer(n) => n.into(),
                rusqlite::types::Value::Real(f) => serde_json::json!(f),
                rusqlite::types::Value::Text(s) => s.into(),
                rusqlite::types::Value::Blob(b) => format!("<blob {} bytes>", b.len()).into(),
            };
            obj.insert(c.clone(), j);
        }
        println!("{}", serde_json::Value::Object(obj));
    }
}
