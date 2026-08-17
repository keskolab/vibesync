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

// ------------------------------------------------------- upload churn

/// A machine with no state for a file the store already holds identically
/// must adopt it, not re-upload it. Re-uploading changes the object's ETag,
/// which invalidates every other machine's listing cache — one machine
/// doing this to ~4,100 objects took every other sync from 8s to 67s.
#[test]
fn push_adopts_identical_objects_instead_of_re_uploading() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    // Machine A publishes two sessions.
    let a = publish_sessions(tmp.path(), &FolderStore::new(store_dir.clone()), "proj", &["s1", "s2"]);

    // Machine B has byte-identical files (it downloaded them earlier) but
    // an EMPTY state — the situation after a state reset or failed applies.
    let b = Machine::new(tmp.path(), "b");
    for u in ["s1", "s2"] {
        b.write_session("proj", u, &format!("session {u}\n"));
    }
    let entries = CLAUDE_CODE.scan(&b.home, &b.tok, false).unwrap();
    let store = FolderStore::new(store_dir);
    let listing = store.list().unwrap();
    let mut state = SyncState::default();

    let r = sync::push_with_listing(&entries, &mut state, &store, "b", &listing).unwrap();

    assert_eq!(r.pushed, 0, "identical content must not be re-uploaded");
    assert_eq!(r.unchanged, 2);
    // Still recorded, so the next sync treats them as known.
    assert_eq!(state.files.len(), 2);
    // And the store still credits the original machine.
    for (_, meta) in store.list().unwrap() {
        assert_eq!(meta.source, "a", "adopting must not restamp ownership");
    }
    let _ = a;
}

/// Genuinely changed content still uploads — the guard must not freeze a
/// file just because an older version is in the store.
#[test]
fn push_still_uploads_when_content_differs() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    publish_sessions(tmp.path(), &FolderStore::new(store_dir.clone()), "proj", &["s1"]);

    let b = Machine::new(tmp.path(), "b");
    b.write_session("proj", "s1", "different content on B\n");
    let entries = CLAUDE_CODE.scan(&b.home, &b.tok, false).unwrap();
    let store = FolderStore::new(store_dir);
    let listing = store.list().unwrap();
    let mut state = SyncState::default();

    let r = sync::push_with_listing(&entries, &mut state, &store, "b", &listing).unwrap();
    assert_eq!(r.pushed, 1, "changed content must still upload");
}

/// Without a listing (the plain push entry point) behaviour is unchanged.
#[test]
fn push_without_a_listing_behaves_as_before() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = Machine::new(tmp.path(), "a");
    a.write_session("proj", "s1", "content\n");
    let entries = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    let mut state = SyncState::default();
    assert_eq!(sync::push(&entries, &mut state, &store, "a").unwrap().pushed, 1);
    // Second push is skipped by state, as always.
    assert_eq!(sync::push(&entries, &mut state, &store, "a").unwrap().pushed, 0);
}

// ------------------------------------------------ end-of-sync diagnosis

fn listed(pairs: &[(&str, &str)]) -> Vec<(String, RemoteMeta)> {
    pairs
        .iter()
        .map(|(k, src)| {
            (
                k.to_string(),
                RemoteMeta {
                    hash: "h".into(),
                    mtime_ms: 1,
                    size: 1,
                    source: src.to_string(),
                },
            )
        })
        .collect()
}

/// The live case seen from the healthy machine: a few objects unreadable,
/// all written by one peer. The log must name that peer as the one to fix.
#[test]
fn diagnosis_names_the_misconfigured_other_machine() {
    let listing = listed(&[
        ("a.jsonl", "mac-mini"),
        ("b.jsonl", "mac-mini"),
        ("c.jsonl", "laptop"),
        ("d.jsonl", "laptop"),
        ("e.jsonl", "laptop"),
        ("f.jsonl", "windows"),
        ("g.jsonl", "windows"),
        ("h.jsonl", "windows"),
    ]);
    let bad = vec!["a.jsonl".to_string(), "b.jsonl".to_string()];
    match sync::diagnose_passphrase(&bad, &listing) {
        sync::PassphraseDiagnosis::OtherMachine { machine, unreadable } => {
            assert_eq!(machine, "mac-mini");
            assert_eq!(unreadable, 2);
        }
        other => panic!("expected OtherMachine, got {other:?}"),
    }
}

/// The same incident seen from the broken machine: it can read almost
/// nothing, so the fix belongs HERE, not on a peer.
#[test]
fn diagnosis_blames_this_machine_when_almost_nothing_is_readable() {
    let listing = listed(&[
        ("a.jsonl", "laptop"),
        ("b.jsonl", "laptop"),
        ("c.jsonl", "windows"),
        ("d.jsonl", "windows"),
        ("e.jsonl", "mac-mini"),
    ]);
    let bad: Vec<String> =
        ["a.jsonl", "b.jsonl", "c.jsonl", "d.jsonl"].iter().map(|s| s.to_string()).collect();
    match sync::diagnose_passphrase(&bad, &listing) {
        sync::PassphraseDiagnosis::ThisMachine { unreadable, total, machines } => {
            assert_eq!(unreadable, 4);
            assert_eq!(total, 5);
            // Sorted by volume so the log leads with the biggest contributor.
            assert_eq!(machines[0].0, "laptop");
            assert_eq!(machines[0].1, 2);
        }
        other => panic!("expected ThisMachine, got {other:?}"),
    }
}

/// Self-heal: objects this machine uploaded and can no longer read were
/// encrypted with a passphrase it no longer uses. The local file is the
/// good copy, so those are the ones to re-upload.
#[test]
fn self_heal_picks_only_this_machines_own_unreadable_objects() {
    let listing = listed(&[
        ("mine-1.jsonl", "mac-mini"),
        ("mine-2.jsonl", "mac-mini.local"), // same machine, hostname suffix
        ("theirs.jsonl", "laptop"),
        ("fine.jsonl", "mac-mini"),
    ]);
    let unreadable = vec![
        "mine-1.jsonl".to_string(),
        "mine-2.jsonl".to_string(),
        "theirs.jsonl".to_string(),
    ];
    let is_me = |s: &str| s.trim_end_matches(".local") == "mac-mini";
    let mine = sync::own_unreadable(&unreadable, &listing, &is_me);
    assert_eq!(mine, vec!["mine-1.jsonl".to_string(), "mine-2.jsonl".to_string()]);
    assert!(!mine.contains(&"theirs.jsonl".to_string()), "another machine's copy is not ours to redo");
    assert!(!mine.contains(&"fine.jsonl".to_string()), "readable objects are untouched");
}

/// The dangerous mistake would be re-uploading over a peer's good copy.
#[test]
fn self_heal_never_touches_another_machines_objects() {
    let listing = listed(&[("a.jsonl", "laptop"), ("b.jsonl", "windows-pc")]);
    let unreadable = vec!["a.jsonl".to_string(), "b.jsonl".to_string()];
    let mine = sync::own_unreadable(&unreadable, &listing, &|s| s == "mac-mini");
    assert!(mine.is_empty(), "a wrong-passphrase peer must be fixed there, not overwritten from here");
}

/// Nothing unreadable, nothing to heal — and an unknown author is not us.
#[test]
fn self_heal_is_empty_on_a_healthy_sync() {
    let listing = listed(&[("a.jsonl", "mac-mini")]);
    assert!(sync::own_unreadable(&[], &listing, &|_| true).is_empty());
    // Key missing from the listing: no author, so no claim of ownership.
    let orphan = vec!["gone.jsonl".to_string()];
    assert!(sync::own_unreadable(&orphan, &listing, &|_| true).is_empty());
}

/// A clean sync says nothing at all — no scary block in a healthy log.
#[test]
fn diagnosis_is_silent_when_everything_is_readable() {
    let listing = listed(&[("a.jsonl", "laptop")]);
    assert_eq!(sync::diagnose_passphrase(&[], &listing), sync::PassphraseDiagnosis::None);
}

/// Objects missing from the listing (deleted mid-sync) must not crash the
/// diagnosis or hide it.
#[test]
fn diagnosis_survives_keys_missing_from_the_listing() {
    let listing = listed(&[("a.jsonl", "laptop")]);
    let bad = vec!["gone.jsonl".to_string()];
    match sync::diagnose_passphrase(&bad, &listing) {
        sync::PassphraseDiagnosis::ThisMachine { machines, .. } => {
            assert_eq!(machines[0].0, "unknown");
        }
        other => panic!("expected ThisMachine, got {other:?}"),
    }
}

/// The collector hands over its keys once and resets, so a later clean
/// sync cannot inherit the previous one's failures.
///
/// The collector is process-global on purpose (the app runs one sync at a
/// time, and rayon workers all report into it), which means sibling tests
/// in this binary share it. So this test uses keys nobody else creates and
/// asserts only about those — an exact global count is not its business.
#[test]
fn unreadable_keys_are_drained_once() {
    const MINE: &str = "drain-probe";
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let keys: Vec<String> = {
        let s = FolderStore::new(store_dir.clone());
        (0..2)
            .map(|i| {
                let logical = format!("claude/projects/{MINE}/{i}.jsonl");
                let body = b"x".to_vec();
                s.put(
                    &logical,
                    &body,
                    &RemoteMeta {
                        hash: vibesync_engine::scanner::hash_bytes(&body),
                        mtime_ms: 1,
                        size: body.len() as u64,
                        source: "a".into(),
                    },
                )
                .unwrap();
                logical
            })
            .collect()
    };
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let store = PoisonStore::undecryptable(store_dir, &refs);
    let mut failed = 0usize;
    for k in &keys {
        sync::fetch_obj(&store, k, &mut failed);
    }
    assert_eq!(failed, 2);

    let drained = sync::take_unreadable();
    let mine: Vec<&String> = drained.iter().filter(|k| k.contains(MINE)).collect();
    assert_eq!(mine.len(), 2, "both keys reported exactly once");
    let again = sync::take_unreadable();
    assert!(
        !again.iter().any(|k| k.contains(MINE)),
        "and the collector resets — a later sync cannot inherit them"
    );
}

// ------------------------------------------- setup-time passphrase check

/// Seed a store with `n` small objects.
fn seed(store: &dyn SyncStore, n: usize) -> Vec<String> {
    let mut keys = Vec::new();
    for i in 0..n {
        let logical = format!("claude/projects/p/{i}.jsonl");
        let body = format!("object {i}\n").into_bytes();
        store
            .put(
                &logical,
                &body,
                &RemoteMeta {
                    hash: vibesync_engine::scanner::hash_bytes(&body),
                    mtime_ms: 1,
                    size: body.len() as u64,
                    source: "a".into(),
                },
            )
            .unwrap();
        keys.push(logical);
    }
    keys
}

/// First machine: nothing to match against, so setup must not cry wolf.
#[test]
fn passphrase_check_reports_new_storage_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    assert_eq!(vibesync_engine::check_passphrase(&store), vibesync_engine::PassphraseCheck::NewStorage);
}

/// Second machine, right phrase: the reassurance that was missing.
#[test]
fn passphrase_check_confirms_a_matching_passphrase() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    seed(&store, 5);
    match vibesync_engine::check_passphrase(&store) {
        vibesync_engine::PassphraseCheck::Matches { sampled } => assert_eq!(sampled, 5),
        other => panic!("expected Matches, got {other:?}"),
    }
}

/// Second machine, wrong phrase — the exact mistake that caused the
/// incident. Setup must be able to say so before the user commits.
#[test]
fn passphrase_check_detects_a_wrong_passphrase() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let keys = {
        let s = FolderStore::new(store_dir.clone());
        seed(&s, 5)
    };
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let store = PoisonStore::undecryptable(store_dir, &refs);
    match vibesync_engine::check_passphrase(&store) {
        vibesync_engine::PassphraseCheck::Mismatch { sampled } => assert_eq!(sampled, 5),
        other => panic!("expected Mismatch, got {other:?}"),
    }
}

/// Our actual store today: this machine's phrase is right, but another
/// machine has been writing under a different one. Saying only "correct"
/// would hide a real problem, so it gets its own verdict.
#[test]
fn passphrase_check_flags_a_store_written_by_two_different_passphrases() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let keys = {
        let s = FolderStore::new(store_dir.clone());
        seed(&s, 6)
    };
    let poisoned: Vec<&str> = keys.iter().take(2).map(|s| s.as_str()).collect();
    let store = PoisonStore::undecryptable(store_dir, &poisoned);
    match vibesync_engine::check_passphrase(&store) {
        vibesync_engine::PassphraseCheck::Mixed { readable, unreadable } => {
            assert_eq!((readable, unreadable), (4, 2));
        }
        other => panic!("expected Mixed, got {other:?}"),
    }
}

/// A store we cannot even list tells us nothing about the passphrase —
/// setup must not block on an unrelated network problem.
#[test]
fn passphrase_check_is_inconclusive_when_the_store_cannot_be_listed() {
    struct Unreachable;
    impl SyncStore for Unreachable {
        fn put(&self, _l: &str, _p: &[u8], _m: &RemoteMeta) -> anyhow::Result<()> {
            Ok(())
        }
        fn get(&self, _l: &str) -> anyhow::Result<Option<(Vec<u8>, RemoteMeta)>> {
            Ok(None)
        }
        fn list(&self) -> anyhow::Result<Vec<(String, RemoteMeta)>> {
            anyhow::bail!("connection reset by peer")
        }
    }
    match vibesync_engine::check_passphrase(&Unreachable) {
        vibesync_engine::PassphraseCheck::Inconclusive { reason } => {
            assert!(reason.contains("connection reset"), "{reason}");
        }
        other => panic!("expected Inconclusive, got {other:?}"),
    }
}

/// Objects that vanished between listing and fetch prove nothing either.
#[test]
fn passphrase_check_is_inconclusive_when_every_sample_vanished() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    {
        let s = FolderStore::new(store_dir.clone());
        seed(&s, 3);
    }
    let store = VanishedStore(FolderStore::new(store_dir));
    assert!(matches!(
        vibesync_engine::check_passphrase(&store),
        vibesync_engine::PassphraseCheck::Inconclusive { .. }
    ));
}

/// The sample is spread across the listing: objects cluster by machine and
/// by namespace, so probing only the first few could sample exclusively
/// from the misconfigured machine and give the wrong verdict.
#[test]
fn passphrase_check_samples_across_the_listing_not_just_the_front() {
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join("store");
    let keys = {
        let s = FolderStore::new(store_dir.clone());
        seed(&s, 40)
    };
    // Poison a contiguous block at the very front of the sorted listing.
    let mut sorted = keys.clone();
    sorted.sort();
    let front: Vec<&str> = sorted.iter().take(8).map(|s| s.as_str()).collect();
    let store = PoisonStore::undecryptable(store_dir, &front);
    match vibesync_engine::check_passphrase(&store) {
        // Spread sampling must still reach readable objects further in.
        vibesync_engine::PassphraseCheck::Mixed { readable, .. } => assert!(readable > 0),
        vibesync_engine::PassphraseCheck::Matches { .. } => {}
        other => panic!("front-loaded poison must not read as a global mismatch: {other:?}"),
    }
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
