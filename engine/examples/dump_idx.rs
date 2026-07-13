//! Throwaway debug helper: print the chat panel index of a state.vscdb.
fn main() {
    let db = std::env::args().nth(1).expect("usage: dump_idx <state.vscdb>");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key='chat.ChatSessionStore.index'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| format!("<<no index key: {e}>>"));
    println!("{v}");
}
