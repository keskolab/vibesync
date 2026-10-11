//! DB merges must keep the README's promises: insert/update-newer only,
//! never delete, one consistent backup before the first write, and a
//! problem with one session costs that session only.
//!
//! Each test pins one way a merge used to break those promises:
//! - `INSERT OR REPLACE` deletes before it inserts, and the bundled SQLite
//!   enforces foreign keys — so replacing a session row fired OpenCode's
//!   `ON DELETE CASCADE` and wiped every local child row of that session.
//! - Merges were not transactional: a failure halfway left the session row
//!   at the newest timestamp, so the retry saw "up to date" and the missing
//!   rows never arrived.
//! - The "backup" was a plain copy of the main db file, which misses
//!   everything still in the WAL.
//! - Child rows were overwritten without asking which copy was newer, and a
//!   "local is newer" verdict skipped remote-only rows entirely.
//! - File-layer applies overwrote the losing local file without a backup.
//! - Optional tables/columns a tool version lacks failed every export.
//! - Codex re-pointed threads whose folder sat on an unplugged drive.

use std::path::{Path, PathBuf};

use vibesync_engine::scanner::hash_bytes;
use vibesync_engine::store::{RemoteMeta, SyncStore};
use vibesync_engine::{codex, copilot, opencode, FolderStore, SyncState, Tokenizer};

// ---------------------------------------------------------------- harness

fn exec(db: &Path, sql: &str) {
    rusqlite::Connection::open(db).unwrap().execute_batch(sql).unwrap();
}

fn count(db: &Path, sql: &str) -> i64 {
    rusqlite::Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn text(db: &Path, sql: &str) -> String {
    rusqlite::Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn ids(db: &Path, sql: &str) -> Vec<String> {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut st = c.prepare(sql).unwrap();
    let v = st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect();
    v
}

fn put(store: &dyn SyncStore, logical: &str, bytes: &[u8], mtime_ms: i64) {
    store
        .put(
            logical,
            bytes,
            &RemoteMeta {
                hash: hash_bytes(bytes),
                mtime_ms,
                size: bytes.len() as u64,
                source: "other".into(),
            },
        )
        .unwrap();
}

fn set_mtime(p: &Path, secs: i64) {
    filetime::set_file_mtime(p, filetime::FileTime::from_unix_time(secs, 0)).unwrap();
}

/// OpenCode's real shape (packages/core/src/session/sql.ts): children
/// reference their session with ON DELETE CASCADE, and the db runs in WAL
/// mode. `todo` stands in for the local-only tables (pending, inbox,
/// instruction, ...) that also cascade from `session` but never sync.
fn opencode_db(home: &Path) -> PathBuf {
    let root = home.join(".local/share/opencode");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("auth.json"), "{}").unwrap();
    let p = root.join("opencode.db");
    exec(
        &p,
        "PRAGMA journal_mode = WAL;
         CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
           time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
           sandboxes TEXT NOT NULL DEFAULT '[]');
         CREATE TABLE session (id TEXT PRIMARY KEY,
           project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
           slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL,
           version TEXT NOT NULL, time_created INTEGER NOT NULL,
           time_updated INTEGER NOT NULL);
         CREATE TABLE message (id TEXT PRIMARY KEY,
           session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
           time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
           data TEXT NOT NULL);
         CREATE TABLE part (id TEXT PRIMARY KEY,
           message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
           session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
           time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
           data TEXT NOT NULL);
         CREATE TABLE todo (
           session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
           content TEXT NOT NULL, position INTEGER NOT NULL,
           PRIMARY KEY (session_id, position));",
    );
    p
}

fn oc_apply(home: &Path, tok: &Tokenizer, st: &mut SyncState, store: &dyn SyncStore) -> vibesync_engine::sync::ApplyReport {
    opencode::db_apply(home, tok, st, store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap()
}

/// Machine A exports session `ses1` (renamed at t=500, two messages).
fn opencode_remote(tmp: &Path, store: &dyn SyncStore) {
    let a = tmp.join("a");
    let db_a = opencode_db(&a);
    exec(
        &db_a,
        "INSERT INTO project VALUES ('prj', '/Users/u/app', 1, 1, '[]');
         INSERT INTO session VALUES ('ses1','prj','s','/Users/u/app','renamed on A','1',100,500);
         INSERT INTO message VALUES ('m1','ses1',100,100,'{\"role\":\"user\"}');
         INSERT INTO part VALUES ('p1','m1','ses1',100,100,'{\"text\":\"hi\"}');
         INSERT INTO message VALUES ('m2','ses1',400,400,'{\"role\":\"assistant\"}');
         INSERT INTO part VALUES ('p2','m2','ses1',400,400,'{\"text\":\"from A\"}');",
    );
    let tok_a = Tokenizer::with_case_sensitivity("/Users/u", false);
    let mut st = SyncState::default();
    assert_eq!(opencode::db_push(&a, &tok_a, &mut st, store, "a", &store.list().unwrap()).unwrap(), 1);
}

// ------------------------------------------------- 1. never cascade-delete

#[test]
fn bundled_sqlite_enforces_foreign_keys() {
    // The premise of the cascade bug: every connection the engine opens
    // enforces foreign keys, so a REPLACE really does fire ON DELETE CASCADE.
    let c = rusqlite::Connection::open_in_memory().unwrap();
    let on: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
    assert_eq!(on, 1);
}

#[test]
fn opencode_remote_newer_merge_keeps_local_only_child_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    // B holds an older copy of ses1 plus rows only B has: a message + part
    // written here, and a local-only todo that cascades from the session.
    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    exec(
        &db_b,
        "INSERT INTO project VALUES ('prj', '/home/bob/app', 1, 1, '[]');
         INSERT INTO session VALUES ('ses1','prj','s','/home/bob/app','old title','1',100,200);
         INSERT INTO message VALUES ('m1','ses1',100,100,'{\"role\":\"user\"}');
         INSERT INTO part VALUES ('p1','m1','ses1',100,100,'{\"text\":\"hi\"}');
         INSERT INTO message VALUES ('m_local','ses1',150,150,'{\"role\":\"user\",\"on\":\"b\"}');
         INSERT INTO part VALUES ('p_local','m_local','ses1',150,150,'{\"text\":\"only on B\"}');
         INSERT INTO todo VALUES ('ses1', 'finish the thing', 0);",
    );
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let r = oc_apply(&b, &tok_b, &mut SyncState::default(), &store);
    assert_eq!((r.applied, r.failed), (1, 0));

    // The newer session row landed...
    assert_eq!(text(&db_b, "SELECT title FROM session WHERE id='ses1'"), "renamed on A");
    // ...the remote's new rows arrived...
    assert_eq!(ids(&db_b, "SELECT id FROM message ORDER BY id"), ["m1", "m2", "m_local"]);
    assert_eq!(ids(&db_b, "SELECT id FROM part ORDER BY id"), ["p1", "p2", "p_local"]);
    // ...and nothing local was cascade-deleted along the way.
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM todo WHERE session_id='ses1'"), 1);
}

// ------------------------------------------------- 2. one transaction each

#[test]
fn opencode_failed_merge_rolls_back_and_lands_on_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    // A failure on the second part — stands in for SQLITE_BUSY, a full
    // disk, or a constraint the remote row breaks, halfway through.
    exec(&db_b, "CREATE TRIGGER boom BEFORE INSERT ON part WHEN NEW.id = 'p2'
                 BEGIN SELECT RAISE(ABORT, 'disk on fire'); END;");
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let mut st = SyncState::default();
    let r = oc_apply(&b, &tok_b, &mut st, &store);
    assert_eq!((r.applied, r.failed), (0, 1), "counted, not fatal");
    // All or nothing: no session row stamped "newest" with half its rows.
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM session"), 0);
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM message"), 0);
    assert!(!st.files.contains_key("opencode/db/ses1.json"), "untracked, so it retries");

    // The cause goes away; the very next sync lands the WHOLE session.
    exec(&db_b, "DROP TRIGGER boom;");
    let r = oc_apply(&b, &tok_b, &mut st, &store);
    assert_eq!((r.applied, r.failed), (1, 0));
    assert_eq!(ids(&db_b, "SELECT id FROM part ORDER BY id"), ["p1", "p2"]);
}

#[test]
fn opencode_one_bad_session_costs_only_that_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);
    // A second, healthy session from another machine.
    let c = tmp.path().join("c");
    let db_c = opencode_db(&c);
    exec(
        &db_c,
        "INSERT INTO project VALUES ('prj2', '/Users/c/other', 1, 1, '[]');
         INSERT INTO session VALUES ('ses2','prj2','s','/Users/c/other','healthy','1',100,100);
         INSERT INTO message VALUES ('mc','ses2',100,100,'{}');",
    );
    let tok_c = Tokenizer::with_case_sensitivity("/Users/c", false);
    opencode::db_push(&c, &tok_c, &mut SyncState::default(), &store, "c", &store.list().unwrap()).unwrap();

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    exec(&db_b, "CREATE TRIGGER boom BEFORE INSERT ON part WHEN NEW.id = 'p2'
                 BEGIN SELECT RAISE(ABORT, 'disk on fire'); END;");
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let r = oc_apply(&b, &tok_b, &mut SyncState::default(), &store);
    assert_eq!((r.applied, r.failed), (1, 1));
    assert_eq!(ids(&db_b, "SELECT id FROM session"), ["ses2"]);
}

// ------------------------------------------------- 3. consistent backups

#[test]
fn opencode_backup_includes_rows_still_in_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    // OpenCode is running: its connection holds recent commits in the WAL
    // (no checkpoint yet), exactly where a plain file copy can't see them.
    let live = rusqlite::Connection::open(&db_b).unwrap();
    live.execute_batch(
        "PRAGMA wal_autocheckpoint = 0;
         INSERT INTO project VALUES ('prjx', '/home/bob/x', 1, 1, '[]');
         INSERT INTO session VALUES ('ses_wal','prjx','s','/home/bob/x','in the WAL','1',1,1);",
    )
    .unwrap();
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let r = oc_apply(&b, &tok_b, &mut SyncState::default(), &store);
    assert_eq!(r.applied, 1);

    let bak = db_b.with_file_name("opencode.db.vibesync-bak");
    assert_eq!(
        count(&bak, "SELECT COUNT(*) FROM session WHERE id = 'ses_wal'"),
        1,
        "backup must be a consistent snapshot including WAL content"
    );
    // ...and it is the PRE-merge state.
    assert_eq!(count(&bak, "SELECT COUNT(*) FROM session WHERE id = 'ses1'"), 0);
    drop(live);
}

#[test]
fn opencode_failed_backup_means_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    // Something squats on the backup's temp name: the snapshot can't be
    // written, so the merge must not touch the db at all.
    let squat = db_b.with_file_name("opencode.db.vibesync-bak.tmp");
    std::fs::create_dir_all(squat.join("occupied")).unwrap();
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let mut st = SyncState::default();
    let r = opencode::db_apply(&b, &tok_b, &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {});
    assert!(r.is_err(), "a merge without its backup is aborted for this sync");
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM session"), 0);
    assert!(!db_b.with_file_name("opencode.db.vibesync-bak").exists());
    assert!(!st.files.contains_key("opencode/db/ses1.json"));
}

#[test]
fn codex_backup_includes_rows_still_in_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let (a, _) = codex_machine(tmp.path(), "a");
    codex_thread(&a, "th1", "From A", 100, 200);
    let tok_a = codex_tok(&a);
    codex::db_push(&a, &tok_a, &mut SyncState::default(), &store, "a", &[]).unwrap();

    let (b, db_b) = codex_machine(tmp.path(), "b");
    codex_rollout(&b, "th1");
    let live = rusqlite::Connection::open(&db_b).unwrap();
    live.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
    drop(codex_thread(&b, "th_wal", "In the WAL", 1, 1));
    let tok_b = codex_tok(&b);
    let r = codex::db_apply(&b, &tok_b, &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
        .unwrap();
    assert_eq!(r.applied, 1);
    let bak = db_b.with_file_name("state_5.sqlite.vibesync-bak");
    assert_eq!(count(&bak, "SELECT COUNT(*) FROM threads WHERE id = 'th_wal'"), 1);
    drop(live);
}

// ------------------------------------------------- 4. per-row newer checks

#[test]
fn opencode_newer_local_part_survives_a_newer_remote_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    // B edited part p1 at t=450 — after A's copy of it (t=100) — but A's
    // session as a whole is newer (t=500), so the session merges.
    exec(
        &db_b,
        "INSERT INTO project VALUES ('prj', '/home/bob/app', 1, 1, '[]');
         INSERT INTO session VALUES ('ses1','prj','s','/home/bob/app','old title','1',100,200);
         INSERT INTO message VALUES ('m1','ses1',100,100,'{\"role\":\"user\"}');
         INSERT INTO part VALUES ('p1','m1','ses1',100,450,'{\"text\":\"edited on B\"}');",
    );
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let r = oc_apply(&b, &tok_b, &mut SyncState::default(), &store);
    assert_eq!(r.applied, 1);
    assert_eq!(text(&db_b, "SELECT data FROM part WHERE id='p1'"), "{\"text\":\"edited on B\"}");
    assert_eq!(text(&db_b, "SELECT title FROM session WHERE id='ses1'"), "renamed on A");
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM part"), 2);
}

#[test]
fn opencode_kept_session_still_gains_remote_only_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    opencode_remote(tmp.path(), &store);

    let b = tmp.path().join("b");
    let db_b = opencode_db(&b);
    // B's copy is newer overall (t=999): it is kept — but A's message m2
    // and its part exist nowhere on B and must still arrive.
    exec(
        &db_b,
        "INSERT INTO project VALUES ('prj', '/home/bob/app', 1, 1, '[]');
         INSERT INTO session VALUES ('ses1','prj','s','/home/bob/app','local title','1',100,999);
         INSERT INTO message VALUES ('m1','ses1',100,100,'{\"role\":\"user\"}');",
    );
    let tok_b = Tokenizer::with_case_sensitivity("/home/bob", false);
    let r = oc_apply(&b, &tok_b, &mut SyncState::default(), &store);
    assert_eq!((r.skipped_newer_local, r.applied), (1, 0));
    assert_eq!(text(&db_b, "SELECT title FROM session WHERE id='ses1'"), "local title");
    assert_eq!(ids(&db_b, "SELECT id FROM message ORDER BY id"), ["m1", "m2"]);
    assert_eq!(ids(&db_b, "SELECT id FROM part ORDER BY id"), ["p1", "p2"]);
}

/// Replica of Copilot CLI's session-store.db (child rows are keyed on
/// natural UNIQUE keys; ids are machine-local AUTOINCREMENT).
fn copilot_db(home: &Path) -> PathBuf {
    let p = home.join(".copilot/session-store.db");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(home.join(".copilot/config.json"), "{}").unwrap();
    exec(
        &p,
        "PRAGMA journal_mode = WAL;
         CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT,
           host_type TEXT, branch TEXT, summary TEXT,
           created_at TEXT DEFAULT (datetime('now')),
           updated_at TEXT DEFAULT (datetime('now')));
         CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
           turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT,
           timestamp TEXT DEFAULT (datetime('now')), UNIQUE(session_id, turn_index));
         CREATE TABLE checkpoints (id INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
           checkpoint_number INTEGER NOT NULL, title TEXT,
           created_at TEXT DEFAULT (datetime('now')),
           UNIQUE(session_id, checkpoint_number));
         CREATE TABLE session_files (id INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
           file_path TEXT NOT NULL, tool_name TEXT, turn_index INTEGER,
           first_seen_at TEXT DEFAULT (datetime('now')),
           UNIQUE(session_id, file_path));
         CREATE TABLE session_refs (id INTEGER PRIMARY KEY AUTOINCREMENT,
           session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
           ref_type TEXT NOT NULL, ref_value TEXT NOT NULL, turn_index INTEGER,
           created_at TEXT DEFAULT (datetime('now')),
           UNIQUE(session_id, ref_type, ref_value));",
    );
    p
}

fn copilot_tok(home: &Path) -> Tokenizer {
    Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows))
}

fn cp_apply(home: &Path, st: &mut SyncState, store: &dyn SyncStore) -> vibesync_engine::sync::ApplyReport {
    copilot::db_apply(home, &copilot_tok(home), st, store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap()
}

/// Both machines hold turns 0..=1 of session `s`, then each continued it.
fn copilot_forked_session(home: &Path, turn2: &str, turn2_at: &str, extra: Option<(&str, &str)>) -> PathBuf {
    let db = copilot_db(home);
    std::fs::create_dir_all(home.join(".copilot/session-state/s")).unwrap();
    exec(
        &db,
        "INSERT INTO sessions (id, summary, created_at, updated_at)
           VALUES ('s', 'shared', '2026-07-13T10:00:00.000Z', '2026-07-13T10:00:00.000Z');
         INSERT INTO turns (session_id, turn_index, user_message, timestamp)
           VALUES ('s', 0, 'zero', '2026-07-13T10:00:00.000Z'),
                  ('s', 1, 'one', '2026-07-13T10:01:00.000Z');",
    );
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO turns (session_id, turn_index, user_message, timestamp) VALUES ('s', 2, ?1, ?2)",
            [turn2, turn2_at],
        )
        .unwrap();
    if let Some((msg, at)) = extra {
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO turns (session_id, turn_index, user_message, timestamp) VALUES ('s', 3, ?1, ?2)",
                [msg, at],
            )
            .unwrap();
    }
    db
}

#[test]
fn copilot_forked_turn_keeps_local_and_adds_remote_only_turns() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    // A continued the session from turn 2 on; its copy is the newer one.
    let a = tmp.path().join("a");
    copilot_forked_session(&a, "A asks", "2026-07-13T10:04:00.000Z", Some(("A again", "2026-07-13T10:05:00.000Z")));
    copilot::db_push(&a, &copilot_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    // B continued the SAME session from the same point: its turn 2 is a
    // different conversation that exists nowhere else.
    let b = tmp.path().join("b");
    let db_b = copilot_forked_session(&b, "B asks", "2026-07-13T10:02:00.000Z", None);
    let r = cp_apply(&b, &mut SyncState::default(), &store);
    assert_eq!((r.applied, r.failed), (1, 0));
    assert_eq!(text(&db_b, "SELECT user_message FROM turns WHERE turn_index = 2"), "B asks");
    // A's turn that B has no counterpart for still arrives.
    assert_eq!(text(&db_b, "SELECT user_message FROM turns WHERE turn_index = 3"), "A again");
}

#[test]
fn copilot_turn_completed_elsewhere_fills_in_locally() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let db_a = copilot_forked_session(&a, "question", "2026-07-13T10:02:00.000Z", None);
    // A finished turn 2 after B had already synced it mid-turn.
    exec(&db_a, "UPDATE turns SET assistant_response = 'the answer' WHERE turn_index = 2;
                 UPDATE sessions SET updated_at = '2026-07-13T10:03:00.000Z';");
    copilot::db_push(&a, &copilot_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    let b = tmp.path().join("b");
    let db_b = copilot_forked_session(&b, "question", "2026-07-13T10:02:00.000Z", None);
    let r = cp_apply(&b, &mut SyncState::default(), &store);
    assert_eq!(r.applied, 1);
    // Same turn, B simply lacked the response: filling a NULL loses nothing.
    assert_eq!(text(&db_b, "SELECT assistant_response FROM turns WHERE turn_index = 2"), "the answer");
}

#[test]
fn copilot_kept_session_still_gains_remote_only_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let db_a = copilot_forked_session(&a, "two", "2026-07-13T10:02:00.000Z", None);
    exec(&db_a, "INSERT INTO session_refs (session_id, ref_type, ref_value, created_at)
                 VALUES ('s', 'pr', '42', '2026-07-13T10:02:30.000Z');");
    copilot::db_push(&a, &copilot_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    // B is newer overall (it kept going), so its copy is kept...
    let b = tmp.path().join("b");
    let db_b = copilot_forked_session(&b, "two", "2026-07-13T10:02:00.000Z", Some(("three", "2026-07-13T11:00:00.000Z")));
    let r = cp_apply(&b, &mut SyncState::default(), &store);
    assert_eq!(r.skipped_newer_local, 1);
    // ...but the reference only A recorded still lands.
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM session_refs WHERE ref_value = '42'"), 1);
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM turns"), 4);
}

// ------------------------------------------------- 5. file-layer backups

fn baks(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".vibesync-bak"))
        .collect();
    v.sort();
    v
}

/// Conflict twice with different local content, then once with content the
/// first backup already holds: `.vibesync-bak`, `.2.vibesync-bak`, reuse.
fn assert_backups_rotate(file: &Path, logical: &str, store: &dyn SyncStore, apply: &dyn Fn() -> vibesync_engine::sync::ApplyReport) {
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    let dir = file.parent().unwrap();
    // Newline-terminated: Codex rollouts are JSONL and materialize per line.
    let rounds: [(&str, &str); 3] =
        [("LOCAL ONE\n", "REMOTE ONE\n"), ("LOCAL TWO\n", "REMOTE TWO\n"), ("LOCAL ONE\n", "REMOTE THREE\n")];
    for (i, (local, remote)) in rounds.iter().enumerate() {
        std::fs::write(file, local).unwrap();
        set_mtime(file, 1_000_000_000 + i as i64);
        put(store, logical, remote.as_bytes(), 1_900_000_000_000 + i as i64);
        let r = apply();
        assert_eq!((r.applied, r.failed), (1, 0), "round {i}");
        assert_eq!(std::fs::read_to_string(file).unwrap(), *remote);
    }
    assert_eq!(baks(dir), [format!("{name}.2.vibesync-bak"), format!("{name}.vibesync-bak")]);
    assert_eq!(std::fs::read_to_string(dir.join(format!("{name}.vibesync-bak"))).unwrap(), "LOCAL ONE\n");
    assert_eq!(std::fs::read_to_string(dir.join(format!("{name}.2.vibesync-bak"))).unwrap(), "LOCAL TWO\n");
}

#[test]
fn opencode_file_apply_backs_up_the_losing_local_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let b = tmp.path().join("b");
    let file = b.join(".local/share/opencode/storage/session/ses_1.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let st = std::cell::RefCell::new(SyncState::default());
    assert_backups_rotate(&file, "opencode/storage/session/ses_1.json", &store, &|| {
        opencode::apply(&b, &mut st.borrow_mut(), &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap()
    });
}

#[test]
fn copilot_file_apply_backs_up_the_losing_local_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let b = tmp.path().join("b");
    let file = b.join(".copilot/session-state/5ebe/workspace.yaml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let st = std::cell::RefCell::new(SyncState::default());
    assert_backups_rotate(&file, "copilot/session-state/5ebe/workspace.yaml", &store, &|| {
        copilot::apply(&b, &mut st.borrow_mut(), &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap()
    });
    // The backups are local safety copies, never content to publish.
    let scanned: Vec<String> = copilot::scan(&b).unwrap().into_iter().map(|e| e.logical).collect();
    assert_eq!(scanned, ["copilot/session-state/5ebe/workspace.yaml"]);
}

#[test]
fn codex_file_apply_backs_up_the_losing_local_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let b = tmp.path().join("b");
    let file = b.join(".codex/sessions/2026/07/12/rollout-x.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let tok = Tokenizer::with_case_sensitivity(&b.to_string_lossy(), false);
    let st = std::cell::RefCell::new(SyncState::default());
    assert_backups_rotate(&file, "codex/sessions/2026/07/12/rollout-x.jsonl", &store, &|| {
        codex::apply(&b, &tok, &mut st.borrow_mut(), &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap()
    });
}

#[test]
fn file_apply_without_a_backup_leaves_the_local_copy_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let b = tmp.path().join("b");
    let cases: [(&str, PathBuf); 3] = [
        ("opencode/storage/session/ses_1.json", b.join(".local/share/opencode/storage/session/ses_1.json")),
        ("copilot/session-state/5ebe/workspace.yaml", b.join(".copilot/session-state/5ebe/workspace.yaml")),
        ("codex/sessions/2026/07/12/rollout-x.jsonl", b.join(".codex/sessions/2026/07/12/rollout-x.jsonl")),
    ];
    for (logical, file) in &cases {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "LOCAL\n").unwrap();
        set_mtime(file, 1_000_000_000);
        // Something that isn't a file occupies the backup name: the local
        // copy can't be kept, so it must not be overwritten either.
        let mut bak = file.file_name().unwrap().to_os_string();
        bak.push(".vibesync-bak");
        std::fs::create_dir_all(file.with_file_name(bak).join("occupied")).unwrap();
        put(&store, logical, b"REMOTE\n", 1_900_000_000_000);
    }
    let tok = Tokenizer::with_case_sensitivity(&b.to_string_lossy(), false);
    let listing = store.list().unwrap();
    let mut st = SyncState::default();
    let r1 = opencode::apply(&b, &mut st, &store, &listing, &|| {}, &|_| {}).unwrap();
    let r2 = copilot::apply(&b, &mut st, &store, &listing, &|| {}, &|_| {}).unwrap();
    let r3 = codex::apply(&b, &tok, &mut st, &store, &listing, &|| {}, &|_| {}).unwrap();
    for (i, r) in [r1, r2, r3].iter().enumerate() {
        assert_eq!((r.applied, r.failed), (0, 1), "case {i}");
    }
    for (logical, file) in &cases {
        assert_eq!(std::fs::read_to_string(file).unwrap(), "LOCAL\n", "{logical}");
        assert!(!st.files.contains_key(*logical), "{logical} stays untracked and retries");
    }
}

// ------------------------------------------------- 8. schema drift on push

/// A Codex generation with only the core thread columns: no ms timestamps,
/// no dynamic-tools or spawn-edge tables.
fn codex_machine(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let home = root.join(name);
    let db = home.join(".codex/state_5.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "").unwrap();
    exec(
        &db,
        "PRAGMA journal_mode = WAL;
         CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
           created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
           cwd TEXT NOT NULL, title TEXT NOT NULL);",
    );
    (home, db)
}

fn codex_tok(home: &Path) -> Tokenizer {
    Tokenizer::with_case_sensitivity(&home.to_string_lossy(), false)
}

fn codex_rollout_path(home: &Path, id: &str) -> PathBuf {
    home.join(format!(".codex/sessions/2026/07/12/rollout-{id}.jsonl"))
}

fn codex_rollout(home: &Path, id: &str) {
    let p = codex_rollout_path(home, id);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "{}\n").unwrap();
}

fn codex_thread(home: &Path, id: &str, title: &str, created: i64, updated: i64) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(home.join(".codex/state_5.sqlite")).unwrap();
    c.execute(
        "INSERT INTO threads VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            codex_rollout_path(home, id).to_string_lossy(),
            created,
            updated,
            home.join("proj").to_string_lossy(),
            title
        ],
    )
    .unwrap();
    c
}

#[test]
fn codex_schema_without_optional_tables_still_exports_and_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let (a, _) = codex_machine(tmp.path(), "a");
    codex_thread(&a, "th1", "Old-schema thread", 100, 200);
    let tok_a = codex_tok(&a);
    assert_eq!(codex::db_push(&a, &tok_a, &mut SyncState::default(), &store, "a", &[]).unwrap(), 1);

    let (b, db_b) = codex_machine(tmp.path(), "b");
    codex_rollout(&b, "th1");
    let tok_b = codex_tok(&b);
    let r = codex::db_apply(&b, &tok_b, &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
        .unwrap();
    assert_eq!((r.applied, r.failed), (1, 0));
    assert_eq!(text(&db_b, "SELECT title FROM threads WHERE id = 'th1'"), "Old-schema thread");
}

#[test]
fn codex_export_with_child_rows_merges_into_a_db_without_those_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    // A newer Codex exported dynamic tools and a spawn edge.
    let obj = serde_json::json!({
        "thread": {"id": "th2", "rollout_path": "${HOME}/.codex/sessions/2026/07/12/rollout-th2.jsonl",
                   "created_at": 100, "updated_at": 200, "updated_at_ms": 200_000,
                   "cwd": "${HOME}/proj", "title": "From a newer Codex"},
        "dynamic_tools": [{"thread_id": "th2", "position": 0, "name": "t", "description": "d", "input_schema": "{}"}],
        "spawn_edges": [{"parent_thread_id": "th0", "child_thread_id": "th2", "status": "open"}],
    });
    put(&store, "codex/db/th2.json", &serde_json::to_vec(&obj).unwrap(), 200_000);
    let (b, db_b) = codex_machine(tmp.path(), "b");
    codex_rollout(&b, "th2");
    let r = codex::db_apply(&b, &codex_tok(&b), &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
        .unwrap();
    assert_eq!((r.applied, r.failed), (1, 0), "missing optional tables are skipped, not fatal");
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM threads WHERE id = 'th2'"), 1);
}

#[test]
fn copilot_schema_without_child_tables_still_exports_and_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let db_a = a.join(".copilot/session-store.db");
    std::fs::create_dir_all(db_a.parent().unwrap()).unwrap();
    // An early generation: sessions + turns only.
    exec(
        &db_a,
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, summary TEXT,
           created_at TEXT, updated_at TEXT);
         CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
           turn_index INTEGER NOT NULL, user_message TEXT, timestamp TEXT,
           UNIQUE(session_id, turn_index));
         INSERT INTO sessions VALUES ('s9', NULL, 'early', '2026-07-13T10:00:00.000Z', '2026-07-13T10:00:00.000Z');
         INSERT INTO turns (session_id, turn_index, user_message, timestamp)
           VALUES ('s9', 0, 'hi', '2026-07-13T10:00:00.000Z');",
    );
    assert_eq!(copilot::db_push(&a, &copilot_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap(), 1);

    // ...merging into a db that lacks checkpoints/session_files/session_refs.
    let b = tmp.path().join("b");
    let db_b = b.join(".copilot/session-store.db");
    std::fs::create_dir_all(b.join(".copilot/session-state/s9")).unwrap();
    exec(
        &db_b,
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, summary TEXT,
           created_at TEXT, updated_at TEXT);
         CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
           turn_index INTEGER NOT NULL, user_message TEXT, timestamp TEXT,
           UNIQUE(session_id, turn_index));",
    );
    let r = cp_apply(&b, &mut SyncState::default(), &store);
    assert_eq!((r.applied, r.failed), (1, 0));
    assert_eq!(text(&db_b, "SELECT user_message FROM turns WHERE session_id = 's9'"), "hi");
}

fn codex_with_tools(home: &Path) {
    exec(
        &home.join(".codex/state_5.sqlite"),
        "CREATE TABLE thread_dynamic_tools (thread_id TEXT NOT NULL
           REFERENCES threads(id) ON DELETE CASCADE, position INTEGER NOT NULL,
           name TEXT NOT NULL, PRIMARY KEY(thread_id, position));",
    );
}

#[test]
fn codex_kept_thread_keeps_its_tools_and_gains_missing_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let (a, db_a) = codex_machine(tmp.path(), "a");
    codex_with_tools(&a);
    codex_thread(&a, "th1", "From A", 100, 200);
    exec(&db_a, "INSERT INTO thread_dynamic_tools VALUES ('th1', 0, 'remote tool'), ('th1', 1, 'extra');");
    codex::db_push(&a, &codex_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    // B's thread is newer; a tool row without its own version collides.
    let (b, db_b) = codex_machine(tmp.path(), "b");
    codex_with_tools(&b);
    codex_rollout(&b, "th1");
    codex_thread(&b, "th1", "Newer on B", 100, 300);
    exec(&db_b, "INSERT INTO thread_dynamic_tools VALUES ('th1', 0, 'local tool');");
    let r = codex::db_apply(&b, &codex_tok(&b), &mut SyncState::default(), &store, &store.list().unwrap(), &|| {}, &|_| {})
        .unwrap();
    assert_eq!(r.skipped_newer_local, 1);
    assert_eq!(text(&db_b, "SELECT name FROM thread_dynamic_tools WHERE position = 0"), "local tool");
    assert_eq!(text(&db_b, "SELECT name FROM thread_dynamic_tools WHERE position = 1"), "extra");
    assert_eq!(text(&db_b, "SELECT title FROM threads WHERE id = 'th1'"), "Newer on B");
}

#[test]
fn codex_one_bad_thread_costs_only_that_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let (a, _) = codex_machine(tmp.path(), "a");
    codex_thread(&a, "th1", "One", 100, 200);
    codex_thread(&a, "th2", "Two", 100, 200);
    codex::db_push(&a, &codex_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    let (b, db_b) = codex_machine(tmp.path(), "b");
    codex_rollout(&b, "th1");
    codex_rollout(&b, "th2");
    exec(&db_b, "CREATE TRIGGER boom BEFORE INSERT ON threads WHEN NEW.id = 'th1'
                 BEGIN SELECT RAISE(ABORT, 'locked'); END;");
    let mut st = SyncState::default();
    let tok_b = codex_tok(&b);
    let r = codex::db_apply(&b, &tok_b, &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap();
    assert_eq!((r.applied, r.failed), (1, 1));
    assert_eq!(ids(&db_b, "SELECT id FROM threads"), ["th2"]);
    // Untracked, so it lands as soon as the cause is gone.
    exec(&db_b, "DROP TRIGGER boom;");
    let r = codex::db_apply(&b, &tok_b, &mut st, &store, &store.list().unwrap(), &|| {}, &|_| {}).unwrap();
    assert_eq!((r.applied, r.failed), (1, 0));
}

#[test]
fn copilot_one_bad_session_costs_only_that_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let db_a = copilot_db(&a);
    exec(
        &db_a,
        "INSERT INTO sessions (id, summary) VALUES ('s1', 'one'), ('s2', 'two');
         INSERT INTO turns (session_id, turn_index, user_message) VALUES ('s1', 0, 'a'), ('s1', 1, 'b');",
    );
    copilot::db_push(&a, &copilot_tok(&a), &mut SyncState::default(), &store, "a", &[]).unwrap();

    let b = tmp.path().join("b");
    let db_b = copilot_db(&b);
    std::fs::create_dir_all(b.join(".copilot/session-state/s1")).unwrap();
    std::fs::create_dir_all(b.join(".copilot/session-state/s2")).unwrap();
    // The SECOND turn of s1 fails: s1 must roll back entirely.
    exec(&db_b, "CREATE TRIGGER boom BEFORE INSERT ON turns WHEN NEW.turn_index = 1
                 BEGIN SELECT RAISE(ABORT, 'locked'); END;");
    let r = cp_apply(&b, &mut SyncState::default(), &store);
    assert_eq!((r.applied, r.failed), (1, 1));
    assert_eq!(ids(&db_b, "SELECT id FROM sessions"), ["s2"]);
    assert_eq!(count(&db_b, "SELECT COUNT(*) FROM turns"), 0);
}

// ------------------------------------------------- 9. no guessing offline

#[test]
fn codex_cwd_on_an_unplugged_drive_is_not_re_pointed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let (b, db_b) = codex_machine(tmp.path(), "b");
    // A folder of the same name exists locally, but the thread's folder
    // lives on an external drive that simply isn't plugged in right now.
    std::fs::create_dir_all(b.join("Documents").join("X")).unwrap();
    let offline = "/Volumes/VibeSync-Unplugged-Test-Drive/work/X";
    codex_thread(&b, "thx", "On the external drive", 100, 200)
        .execute("UPDATE threads SET cwd = ?1 WHERE id = 'thx'", [offline])
        .unwrap();
    codex::db_apply(&b, &codex_tok(&b), &mut SyncState::default(), &store, &[], &|| {}, &|_| {}).unwrap();
    assert_eq!(text(&db_b, "SELECT cwd FROM threads WHERE id = 'thx'"), offline);
}

// ------------------------------------------------ self-heal re-uploads

/// Mark every store object under `prefix` for re-upload, as the self-heal
/// check does; returns how many.
fn queue_reupload(st: &mut SyncState, store: &dyn SyncStore, prefix: &str) -> usize {
    let keys: Vec<String> =
        store.list().unwrap().into_iter().map(|(k, _)| k).filter(|k| k.starts_with(prefix)).collect();
    assert!(!keys.is_empty(), "nothing exported under {prefix}");
    vibesync_engine::sync::mark_for_reupload(st, &keys);
    keys.len()
}

/// A queued self-heal re-upload must go out even though the store's
/// recorded hash equals the local export: each adapter's "the store already
/// has it" shortcut honours the mark, then settles again.
#[test]
fn db_exports_queued_for_reupload_are_resent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));

    // OpenCode
    let oc = tmp.path().join("oc");
    exec(
        &opencode_db(&oc),
        "INSERT INTO project VALUES ('prj', '/Users/u/app', 1, 1, '[]');
         INSERT INTO session VALUES ('ses1','prj','s','/Users/u/app','t','1',100,500);",
    );
    let tok = Tokenizer::with_case_sensitivity("/Users/u", false);
    let mut st = SyncState::default();
    let push = |st: &mut SyncState| opencode::db_push(&oc, &tok, st, &store, "a", &store.list().unwrap()).unwrap();
    assert_eq!(push(&mut st), 1);
    assert_eq!(push(&mut st), 0, "settled");
    assert_eq!(queue_reupload(&mut st, &store, opencode::DB_PREFIX), 1);
    assert_eq!(push(&mut st), 1, "re-sent");
    assert_eq!(push(&mut st), 0);

    // Codex (thread export)
    let (cx, _) = codex_machine(tmp.path(), "cx");
    codex_rollout(&cx, "th1");
    codex_thread(&cx, "th1", "One", 100, 200);
    let ctok = codex_tok(&cx);
    let mut st = SyncState::default();
    let push = |st: &mut SyncState| codex::db_push(&cx, &ctok, st, &store, "a", &store.list().unwrap()).unwrap();
    assert_eq!(push(&mut st), 1);
    assert_eq!(push(&mut st), 0);
    queue_reupload(&mut st, &store, codex::DB_PREFIX);
    assert_eq!(push(&mut st), 1);
    assert_eq!(push(&mut st), 0);

    // Copilot CLI
    let cp = tmp.path().join("cp");
    exec(&copilot_db(&cp), "INSERT INTO sessions (id, summary) VALUES ('s1', 'one');");
    let ptok = copilot_tok(&cp);
    let mut st = SyncState::default();
    let push = |st: &mut SyncState| copilot::db_push(&cp, &ptok, st, &store, "a", &store.list().unwrap()).unwrap();
    assert_eq!(push(&mut st), 1);
    assert_eq!(push(&mut st), 0);
    queue_reupload(&mut st, &store, copilot::DB_PREFIX);
    assert_eq!(push(&mut st), 1);
    assert_eq!(push(&mut st), 0);
}
