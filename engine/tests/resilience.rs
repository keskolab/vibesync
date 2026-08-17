//! One bad object must never sink the sync.
//!
//! Live incident (2026-08-17): four objects in a shared store had been
//! written by a machine using a different passphrase. Every apply path
//! fetched objects with `store.get(logical)?`, so the first undecryptable
//! object aborted the entire pass — ~10,700 healthy objects, the Claude
//! sidebar, and every session for every project stopped applying on two
//! machines, every sync, silently. The symptom read as "sync is completely
//! dead" rather than "four files are unreadable".
//!
//! Every fetch now goes through `sync::fetch_obj`: count it, log it, skip
//! it, keep going — and leave state untracked so it retries by itself.
//! These tests pin that behaviour for the generic file layer, the file
//! adapters, and the db-merge adapters.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use vibesync_engine::adapters::CLAUDE_CODE;
use vibesync_engine::store::{RemoteMeta, SyncStore};
use vibesync_engine::tokenizer::encode_cwd;
use vibesync_engine::{sync, FolderStore, SyncState, Tokenizer};

// ---------------------------------------------------------------- harness

/// Wraps a real store and fails `get` for chosen keys. Models both an
/// object written under a different passphrase (permanent, the live case)
/// and a transient network error — the engine must treat them the same.
struct PoisonStore {
    inner: FolderStore,
    poison: Mutex<HashSet<String>>,
    message: String,
}

impl PoisonStore {
    fn new(dir: PathBuf, poison: &[&str], message: &str) -> Self {
        Self {
            inner: FolderStore::new(dir),
            poison: Mutex::new(poison.iter().map(|s| s.to_string()).collect()),
            message: message.to_string(),
        }
    }

    /// The real message from the incident log.
    fn undecryptable(dir: PathBuf, poison: &[&str]) -> Self {
        Self::new(dir, poison, "age decrypt (both key modes failed): No matching keys found")
    }

    fn heal(&self) {
        self.poison.lock().unwrap().clear();
    }
}

impl SyncStore for PoisonStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> anyhow::Result<()> {
        self.inner.put(logical, plain, meta)
    }
    fn get(&self, logical: &str) -> anyhow::Result<Option<(Vec<u8>, RemoteMeta)>> {
        if self.poison.lock().unwrap().contains(logical) {
            anyhow::bail!("decode {logical}: {}", self.message);
        }
        self.inner.get(logical)
    }
    fn list(&self) -> anyhow::Result<Vec<(String, RemoteMeta)>> {
        self.inner.list()
    }
}

/// A store whose objects all vanished between listing and fetch.
struct VanishedStore(FolderStore);

impl SyncStore for VanishedStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> anyhow::Result<()> {
        self.0.put(logical, plain, meta)
    }
    fn get(&self, _logical: &str) -> anyhow::Result<Option<(Vec<u8>, RemoteMeta)>> {
        Ok(None)
    }
    fn list(&self) -> anyhow::Result<Vec<(String, RemoteMeta)>> {
        self.0.list()
    }
}

struct Machine {
    home: PathBuf,
    tok: Tokenizer,
    state: SyncState,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Self {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        let tok = Tokenizer::new(&home.to_string_lossy());
        Machine { home, tok, state: SyncState::default() }
    }

    fn write_session(&self, project: &str, uuid: &str, content: &str) -> PathBuf {
        let cwd = format!("{}/{}", self.home.to_string_lossy(), project);
        let dir = self.home.join(".claude").join("projects").join(encode_cwd(&cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{uuid}.jsonl"));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn session_path(&self, project: &str, uuid: &str) -> PathBuf {
        let cwd = format!("{}/{}", self.home.to_string_lossy(), project);
        self.home
            .join(".claude")
            .join("projects")
            .join(encode_cwd(&cwd))
            .join(format!("{uuid}.jsonl"))
    }
}

/// Machine A publishes `uuids` sessions of one project into `store`.
fn publish_sessions(root: &Path, store: &dyn SyncStore, project: &str, uuids: &[&str]) -> Machine {
    let a = Machine::new(root, "a");
    for u in uuids {
        a.write_session(project, u, &format!("session {u}\n"));
    }
    let entries = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    let mut state = SyncState::default();
    sync::push(&entries, &mut state, store, "a").unwrap();
    a
}

/// The real logical key of a published session — read from the store, not
/// guessed: keys are tokenized (`${EHOME}`/`${GIT:…}`), so reconstructing
/// them from a local path would silently miss and make a poison test pass
/// for the wrong reason.
fn store_key(store_dir: &Path, uuid: &str) -> String {
    let needle = format!("{uuid}.jsonl");
    FolderStore::new(store_dir.to_path_buf())
        .list()
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .find(|k| k.ends_with(&needle))
        .unwrap_or_else(|| panic!("no store key ends with {needle}"))
}

fn pull(b: &mut Machine, store: &dyn SyncStore) -> sync::Report {
    let listing = store.list().unwrap();
    sync::pull_dir(
        &CLAUDE_CODE,
        &b.home,
        ".claude",
        &b.tok,
        &mut b.state,
        store,
        false,
        &|_| false,
        &listing,
        &|| {},
        &|_| {},
    )
    .unwrap()
}

// ------------------------------------------------- generic file layer

/// The incident, minimized: one unreadable object among healthy ones.
#[test]
fn one_unreadable_object_never_blocks_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(tmp.path().join("store")),
        "proj",
        &["s1", "s2", "s3", "s4", "s5"],
    );
    let poisoned = store_key(&tmp.path().join("store"), "s3");
    let store = PoisonStore::undecryptable(tmp.path().join("store"), &[&poisoned]);

    let mut b = Machine::new(tmp.path(), "b");
    let r = pull(&mut b, &store);

    assert_eq!(r.pulled, 4, "every healthy session must still apply");
    assert_eq!(r.failed, 1, "the unreadable one is counted, not fatal");
    for u in ["s1", "s2", "s4", "s5"] {
        assert!(b.session_path("proj", u).exists(), "{u} should have landed");
    }
    assert!(!b.session_path("proj", "s3").exists(), "the unreadable one must not appear");
}

/// The failure must not depend on where the bad object sits in the batch —
/// the old `collect::<Result<_>>()?` aborted wherever it hit.
#[test]
fn poison_at_both_ends_of_the_batch_still_applies_the_middle() {
    let tmp = tempfile::tempdir().unwrap();
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(tmp.path().join("store")),
        "proj",
        &["s1", "s2", "s3", "s4", "s5"],
    );
    let first = store_key(&tmp.path().join("store"), "s1");
    let last = store_key(&tmp.path().join("store"), "s5");
    let store = PoisonStore::undecryptable(tmp.path().join("store"), &[&first, &last]);

    let mut b = Machine::new(tmp.path(), "b");
    let r = pull(&mut b, &store);

    assert_eq!(r.pulled, 3);
    assert_eq!(r.failed, 2);
    for u in ["s2", "s3", "s4"] {
        assert!(b.session_path("proj", u).exists());
    }
}

/// Total unreadability is still a completed sync, not an error: an Err here
/// marks the whole tool failed and hides everything else it did.
#[test]
fn every_object_unreadable_is_still_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(tmp.path().join("store")),
        "proj",
        &["s1", "s2", "s3"],
    );
    let sd = tmp.path().join("store");
    let keys: Vec<String> = ["s1", "s2", "s3"].iter().map(|u| store_key(&sd, u)).collect();
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let store = PoisonStore::undecryptable(tmp.path().join("store"), &refs);

    let mut b = Machine::new(tmp.path(), "b");
    let r = pull(&mut b, &store);

    assert_eq!(r.pulled, 0);
    assert_eq!(r.failed, 3);
    assert!(b.state.files.is_empty(), "nothing may be recorded as synced");
}

/// The whole point of not recording state: the object lands by itself once
/// the passphrase is fixed, with no reset or re-onboarding.
#[test]
fn unreadable_object_is_retried_and_lands_once_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(tmp.path().join("store")),
        "proj",
        &["s1", "s2"],
    );
    let poisoned = store_key(&tmp.path().join("store"), "s2");
    let store = PoisonStore::undecryptable(tmp.path().join("store"), &[&poisoned]);

    let mut b = Machine::new(tmp.path(), "b");
    let first = pull(&mut b, &store);
    assert_eq!((first.pulled, first.failed), (1, 1));
    assert!(!b.session_path("proj", "s2").exists());

    // The other machine's passphrase is corrected; the object becomes readable.
    store.heal();
    let second = pull(&mut b, &store);
    assert_eq!(second.pulled, 1, "the retry must pick it up");
    assert_eq!(second.failed, 0);
    assert!(b.session_path("proj", "s2").exists());
    // And a third sync settles — no churn.
    let third = pull(&mut b, &store);
    assert_eq!((third.pulled, third.failed), (0, 0));
    assert_eq!(third.unchanged, 2);
}

/// A failure must leave no trace: no state entry (or the retry above never
/// happens), no tombstone, and no half-written temp file.
#[test]
fn a_failed_fetch_records_no_state_and_leaves_no_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(tmp.path().join("store")),
        "proj",
        &["s1"],
    );
    let poisoned = store_key(&tmp.path().join("store"), "s1");
    let store = PoisonStore::undecryptable(tmp.path().join("store"), &[&poisoned]);

    let mut b = Machine::new(tmp.path(), "b");
    pull(&mut b, &store);

    assert!(b.state.files.get(&poisoned).is_none(), "no state for a failed object");
    let stray: Vec<_> = walkdir::WalkDir::new(&b.home)
        .into_iter()
        .flatten()
        .filter(|e| e.path().to_string_lossy().contains("vibesync-tmp"))
        .collect();
    assert!(stray.is_empty(), "no temp residue: {stray:?}");
}

/// "Nothing is ever lost" still holds when the fetch dies: the local copy
/// must survive untouched, never be truncated or replaced by nothing.
#[test]
fn a_failed_fetch_never_damages_the_local_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let _a = publish_sessions(tmp.path(), &FolderStore::new(store_dir.clone()), "proj", &["s1"]);
    let poisoned = store_key(&tmp.path().join("store"), "s1");

    // B has an OLDER copy of the same session — normally it would be
    // backed up and replaced by the store's newer content.
    let mut b = Machine::new(tmp.path(), "b");
    let local = b.write_session("proj", "s1", "local copy that must survive\n");
    filetime::set_file_mtime(&local, filetime::FileTime::from_unix_time(1, 0)).unwrap();

    let store = PoisonStore::undecryptable(store_dir, &[&poisoned]);
    let r = pull(&mut b, &store);

    assert_eq!(r.failed, 1);
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "local copy that must survive\n",
        "a failed fetch must not modify local content"
    );
}

/// An object that disappeared from the store between listing and fetch is
/// not a failure — nothing to count, nothing to warn about.
#[test]
fn object_that_vanished_from_the_store_is_not_counted_as_failed() {
    let tmp = tempfile::tempdir().unwrap();
    publish_sessions(tmp.path(), &FolderStore::new(tmp.path().join("store")), "proj", &["s1", "s2"]);
    let store = VanishedStore(FolderStore::new(tmp.path().join("store")));

    let mut b = Machine::new(tmp.path(), "b");
    let r = pull(&mut b, &store);

    assert_eq!(r.pulled, 0);
    assert_eq!(r.failed, 0, "a missing object is not an unreadable one");
}

/// Every outcome keeps its own bucket in one pass — a mixed reality sync.
#[test]
fn unreadable_parked_and_newer_local_each_land_in_their_own_bucket() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let _a = publish_sessions(
        tmp.path(),
        &FolderStore::new(store_dir.clone()),
        "proj",
        &["ok1", "ok2", "bad", "newer"],
    );
    let poisoned = store_key(&store_dir, "bad");
    let store = PoisonStore::undecryptable(store_dir, &[&poisoned]);

    let mut b = Machine::new(tmp.path(), "b");
    // A locally-newer copy of one session must still win.
    let local = b.write_session("proj", "newer", "newer local wins\n");
    filetime::set_file_mtime(
        &local,
        filetime::FileTime::from_unix_time(chrono_now_secs() + 3600, 0),
    )
    .unwrap();

    let r = pull(&mut b, &store);

    assert_eq!(r.pulled, 2, "ok1 + ok2");
    assert_eq!(r.failed, 1, "bad");
    assert_eq!(r.skipped_newer_local, 1, "newer");
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "newer local wins\n",
        "newer-local must be untouched by a neighbouring failure"
    );
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A destination we cannot write (locked-down folder) is also per-object:
/// the write failure costs that file, not the pass.
#[cfg(unix)]
#[test]
fn an_unwritable_destination_costs_only_that_file() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    // Two projects: one whose destination is locked, one that must land.
    let _a = publish_sessions(tmp.path(), &FolderStore::new(store_dir.clone()), "locked", &["s1"]);
    {
        // Publish a second project from the same machine A home.
        let a2 = Machine::new(tmp.path(), "a");
        a2.write_session("open", "s2", "second project\n");
        let entries = CLAUDE_CODE.scan(&a2.home, &a2.tok, false).unwrap();
        let mut st = SyncState::default();
        sync::push(&entries, &mut st, &FolderStore::new(store_dir.clone()), "a").unwrap();
    }

    let mut b = Machine::new(tmp.path(), "b");
    // B's destination for the "locked" project, read-only so the write fails.
    let cwd = format!("{}/{}", b.home.to_string_lossy(), "locked");
    let dir = b.home.join(".claude").join("projects").join(encode_cwd(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let store = FolderStore::new(store_dir);
    let r = pull(&mut b, &store);

    // Restore permissions so tempdir cleanup works.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(r.pulled, 1, "the writable project still lands");
    assert_eq!(r.failed, 1, "the unwritable file is counted, not fatal");
    assert!(b.session_path("open", "s2").exists());
    assert!(
        !b.state.files.keys().any(|k| k.ends_with("s1.jsonl")),
        "an unwritten file must not be recorded as synced"
    );
}

/// The classifier drives the actionable log line, so it must recognise the
/// real message and not cry wolf on ordinary I/O trouble.
#[test]
fn undecryptable_is_told_apart_from_transient_trouble() {
    assert!(sync::is_undecryptable(
        "decode claude/meta/settings.json: age decrypt (both key modes failed): No matching keys found"
    ));
    assert!(sync::is_undecryptable("No matching keys found"));
    assert!(!sync::is_undecryptable("connection reset by peer"));
    assert!(!sync::is_undecryptable("HTTP status 503"));
}

// ------------------------------------------------------- file adapters

/// Copilot CLI's session-state file layer.
#[test]
fn copilot_file_layer_skips_only_the_unreadable_object() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    {
        let store = FolderStore::new(store_dir.clone());
        for (name, body) in [("s1", b"one".as_slice()), ("s2", b"two".as_slice())] {
            let logical = format!("copilot/session-state/{name}/workspace.yaml");
            store
                .put(
                    &logical,
                    body,
                    &RemoteMeta {
                        hash: vibesync_engine::scanner::hash_bytes(body),
                        mtime_ms: 1,
                        size: body.len() as u64,
                        source: "a".into(),
                    },
                )
                .unwrap();
        }
    }
    let store =
        PoisonStore::undecryptable(store_dir, &["copilot/session-state/s1/workspace.yaml"]);
    let home = tmp.path().join("b");
    std::fs::create_dir_all(home.join(".copilot")).unwrap();
    let mut state = SyncState::default();

    let r = vibesync_engine::copilot::apply(
        &home,
        &mut state,
        &store,
        &store.list().unwrap(),
        &|| {},
        &|_| {},
    )
    .unwrap();

    assert_eq!(r.applied, 1);
    assert_eq!(r.failed, 1);
    assert!(home.join(".copilot/session-state/s2/workspace.yaml").exists());
    assert!(!home.join(".copilot/session-state/s1/workspace.yaml").exists());
}

/// OpenCode's storage file layer.
#[test]
fn opencode_file_layer_skips_only_the_unreadable_object() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    {
        let store = FolderStore::new(store_dir.clone());
        for name in ["ses_a", "ses_b"] {
            let logical = format!("opencode/storage/session/{name}.json");
            let body = b"{}";
            store
                .put(
                    &logical,
                    body,
                    &RemoteMeta {
                        hash: vibesync_engine::scanner::hash_bytes(body),
                        mtime_ms: 1,
                        size: body.len() as u64,
                        source: "a".into(),
                    },
                )
                .unwrap();
        }
    }
    let store = PoisonStore::undecryptable(store_dir, &["opencode/storage/session/ses_a.json"]);
    let home = tmp.path().join("b");
    std::fs::create_dir_all(&home).unwrap();
    let mut state = SyncState::default();

    let r = vibesync_engine::opencode::apply(
        &home,
        &mut state,
        &store,
        &store.list().unwrap(),
        &|| {},
        &|_| {},
    )
    .unwrap();

    assert_eq!(r.applied, 1);
    assert_eq!(r.failed, 1);
}

// --------------------------------------------------------- db adapters

/// Codex's thread-db merge: one unreadable export must not stop the other
/// threads from merging into the local sqlite db.
#[test]
fn codex_db_merge_skips_only_the_unreadable_export() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("b");
    let db = home.join(".codex/state_5.sqlite");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL,
           created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
           source TEXT NOT NULL, model_provider TEXT NOT NULL, cwd TEXT NOT NULL,
           title TEXT NOT NULL, sandbox_policy TEXT NOT NULL, approval_mode TEXT NOT NULL,
           created_at_ms INTEGER, updated_at_ms INTEGER, recency_at_ms INTEGER NOT NULL DEFAULT 0,
           git_origin_url TEXT);
         CREATE TABLE thread_dynamic_tools (thread_id TEXT NOT NULL, position INTEGER NOT NULL,
           name TEXT NOT NULL, description TEXT NOT NULL, input_schema TEXT NOT NULL,
           PRIMARY KEY(thread_id, position));
         CREATE TABLE thread_spawn_edges (parent_thread_id TEXT NOT NULL,
           child_thread_id TEXT NOT NULL PRIMARY KEY, status TEXT NOT NULL);",
    )
    .unwrap();
    drop(conn);

    let store_dir = tmp.path().join("store");
    {
        let store = FolderStore::new(store_dir.clone());
        for id in ["t1", "t2"] {
            // No rollout_path: the merge gate is satisfied trivially.
            let obj = serde_json::json!({
                "thread": {
                    "id": id, "rollout_path": "", "created_at": 1, "updated_at": 2,
                    "source": "s", "model_provider": "p", "cwd": home.to_string_lossy(),
                    "title": id, "sandbox_policy": "{}", "approval_mode": "on-request",
                    "updated_at_ms": 2000
                },
                "dynamic_tools": [], "spawn_edges": []
            });
            let bytes = serde_json::to_vec(&obj).unwrap();
            store
                .put(
                    &format!("codex/db/{id}.json"),
                    &bytes,
                    &RemoteMeta {
                        hash: vibesync_engine::scanner::hash_bytes(&bytes),
                        mtime_ms: 2000,
                        size: bytes.len() as u64,
                        source: "a".into(),
                    },
                )
                .unwrap();
        }
    }
    let store = PoisonStore::undecryptable(store_dir, &["codex/db/t1.json"]);
    let tok = Tokenizer::new(&home.to_string_lossy());
    let mut state = SyncState::default();

    let r = vibesync_engine::codex::db_apply(
        &home,
        &tok,
        &mut state,
        &store,
        &store.list().unwrap(),
        &|| {},
        &|_| {},
    )
    .unwrap();

    assert_eq!(r.applied, 1, "the readable thread still merges");
    assert_eq!(r.failed, 1);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM threads ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(ids, vec!["t2".to_string()]);
}
