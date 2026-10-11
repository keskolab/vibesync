//! Regression tests for sync-safety bugs found in the 2026-10 review. Each
//! drives the real engine through the same steps the app runs per sync:
//! scan, one store listing, self-heal check, newer-store keep-back, push,
//! pull.

use std::path::{Path, PathBuf};

use vibesync_engine::adapters::CLAUDE_CODE;
use vibesync_engine::tokenizer::encode_cwd;
use vibesync_engine::{sync, AgeCodec, FolderStore, SyncState, SyncStore, Tokenizer};

struct Machine {
    name: &'static str,
    home: PathBuf,
    tok: Tokenizer,
    state: SyncState,
}

impl Machine {
    fn new(root: &Path, name: &'static str) -> Self {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        Machine { name, tok: Tokenizer::new(&home.to_string_lossy()), home, state: SyncState::default() }
    }

    fn project_dir(&self, project: &str) -> PathBuf {
        let cwd = format!("{}/{project}", self.home.to_string_lossy());
        self.home.join(".claude/projects").join(encode_cwd(&cwd))
    }

    fn session(&self) -> PathBuf {
        self.project_dir("dev/proj").join("s.jsonl")
    }

    fn write(&self, content: &str, mtime_s: i64) {
        write_at(&self.session(), content, mtime_s);
    }

    fn read(&self) -> String {
        std::fs::read_to_string(self.session()).unwrap()
    }

    /// One sync, in the app's order (app/src-tauri/src/syncer.rs).
    fn sync(&mut self, store: &dyn SyncStore) -> sync::Report {
        let mut entries = CLAUDE_CODE.scan(&self.home, &self.tok, false).unwrap();
        let listing = store.list().unwrap();
        if let Some(key) = store.key_id() {
            if self.state.own_checked_key.as_deref() != Some(key.as_str()) {
                let me = self.name;
                let check = sync::stale_own_uploads(store, &listing, &|s| s == me).unwrap();
                if check.key_matches_fleet() {
                    sync::mark_for_reupload(&mut self.state, &check.stale);
                }
                self.state.own_checked_key = Some(key);
            }
        }
        entries.retain(|e| {
            listing
                .iter()
                .find(|(k, _)| *k == e.logical)
                .map(|(_, m)| m.hash == e.hash || m.mtime_ms <= e.mtime_ms)
                .unwrap_or(true)
        });
        let push = sync::push_with_listing(&entries, &mut self.state, store, self.name, &listing).unwrap();
        let pull = sync::pull_dir(
            &CLAUDE_CODE, &self.home, ".claude", &self.tok, &mut self.state, store, false,
            &|_| false, &listing, &|| {}, &|_| {},
        )
        .unwrap();
        sync::Report { pushed: push.pushed, pulled: pull.pulled, failed: push.failed + pull.failed, ..Default::default() }
    }
}

fn write_at(path: &Path, content: &str, mtime_s: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime_s, 0)).unwrap();
}

fn find(root: &Path, suffix: &str) -> PathBuf {
    walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .find(|e| e.path().to_string_lossy().ends_with(suffix))
        .unwrap_or_else(|| panic!("no *{suffix} under {}", root.display()))
        .into_path()
}

/// Cloud-folder clients deliver an object's tiny `.meta.json` and its large
/// data file independently. New metadata over old data used to be applied
/// as the new version, and then pushed back up as newest — reverting the
/// session on the machine that wrote it.
#[test]
fn torn_meta_and_data_never_regress_the_newest_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = FolderStore::new(&root);
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");

    a.write("v1\n", 1_000);
    a.sync(&store);
    b.sync(&store);
    assert_eq!(b.read(), "v1\n");

    a.write("v1\nv2\n", 2_000);
    let old_data = std::fs::read(find(&root, ".jsonl.gz")).unwrap();
    a.sync(&store);
    let new_data = std::fs::read(find(&root, ".jsonl.gz")).unwrap();

    // On B's disk the new metadata has arrived, the data has not.
    std::fs::write(find(&root, ".jsonl.gz"), &old_data).unwrap();
    b.sync(&store);
    assert_eq!(b.read(), "v1\n", "half-arrived object must not be applied");
    // The data catches up.
    std::fs::write(find(&root, ".jsonl.gz"), &new_data).unwrap();
    b.sync(&store);
    a.sync(&store);

    assert_eq!(a.read(), "v1\nv2\n");
    assert_eq!(b.read(), "v1\nv2\n");
}

/// Switching a scope off for a sync used to mark every tracked file in it
/// deleted (the deletion mark is one-way), so after switching it back on
/// this machine never received another update to those files.
#[test]
fn a_file_marked_deleted_while_present_still_receives_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("v1\n", 1_000);
    a.sync(&store);
    b.sync(&store);

    // State left behind by older versions: everything under the scope marked.
    b.state.mark_deletions("claude/projects", &[]);

    a.write("v1\nv2\n", 2_000);
    a.sync(&store);
    b.sync(&store);
    assert_eq!(b.read(), "v1\nv2\n");
}

/// A file restored after being marked deleted must not be re-uploaded when
/// the store already holds those bytes (it would only churn every other
/// machine's listing cache).
#[test]
fn a_restored_file_is_adopted_not_reuploaded() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    a.write("v1\n", 1_000);
    a.sync(&store);
    a.state.mark_deletions("claude/projects", &[]);
    let r = a.sync(&store);
    assert_eq!(r.pushed, 0);
    assert!(a.state.files.values().all(|st| !st.deleted_locally));
}

/// The documented self-heal: uploads made under a mistyped passphrase are
/// re-sent once the passphrase is corrected — including files that never
/// change again. It used to heal nothing: a machine never fetches its own
/// objects, and a forgotten upload was "adopted" straight back.
#[test]
fn correcting_the_passphrase_reuploads_this_machines_unreadable_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("hello\n", 1_000);

    a.sync(&FolderStore::with_codec(&root, Box::new(AgeCodec::new("typo"))));

    let right = FolderStore::with_codec(&root, Box::new(AgeCodec::new("right")));
    let r = a.sync(&right);
    assert_eq!(r.pushed, 1, "the stale upload is re-sent on the first sync");
    let r = b.sync(&right);
    assert_eq!((r.pulled, r.failed), (1, 0));
    assert_eq!(b.read(), "hello\n");
    // Settles: nothing more to re-send.
    assert_eq!(a.sync(&right).pushed, 0);
}

/// A second conflict on the same file used to overwrite the first backup.
#[test]
fn every_losing_version_keeps_its_own_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("base\n", 1_000);
    a.sync(&store);
    b.sync(&store);

    for (round, t) in [(1, 2_000), (2, 4_000)] {
        b.write(&format!("b-only-{round}\n"), t);
        a.write(&format!("a-wins-{round}\n"), t + 1_000);
        a.sync(&store);
        b.sync(&store);
    }
    let dir = b.session().parent().unwrap().to_path_buf();
    let mut baks: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".vibesync-bak"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    baks.sort();
    assert_eq!(baks, vec!["b-only-1\n", "b-only-2\n"]);
}

/// Claude's retention cleanup deletes a session here; later its project is
/// re-keyed (here: it moves to a different project folder name, as when a
/// project name is assigned). Another machine uploads the same old session
/// under the new key. It must not come back.
#[test]
fn a_cleaned_up_session_stays_gone_after_its_project_is_rekeyed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("old session\n", 1_000);
    a.sync(&store);
    b.sync(&store);

    // B's tool cleans it up; B records the deletion.
    std::fs::remove_file(b.session()).unwrap();
    let scan = CLAUDE_CODE.scan(&b.home, &b.tok, false).unwrap();
    b.state.mark_deletions("claude/projects", &scan);
    b.sync(&store);

    // A's copy now lives under a different project key.
    let moved = a.project_dir("dev/renamed").join("s.jsonl");
    write_at(&moved, "old session\n", 1_000);
    std::fs::remove_file(a.session()).unwrap();
    a.sync(&store);

    let r = b.sync(&store);
    assert_eq!(r.pulled, 0);
    assert!(!b.project_dir("dev/renamed").join("s.jsonl").exists());
}

/// One file the scan can't read used to fail every sync, and once readable
/// again it must not have been recorded as deleted.
#[cfg(unix)]
#[test]
fn an_unreadable_file_costs_that_file_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    a.write("v1\n", 1_000);
    // A subagent transcript folder that becomes unreadable (a file is
    // served from the hash cache without a read, so lock the folder).
    let locked = a.project_dir("dev/proj").join("s");
    write_at(&locked.join("subagents/agent-1.jsonl"), "x\n", 1_000);
    a.sync(&store);
    assert_eq!(a.state.files.len(), 2);

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        return; // running as root: permissions don't apply
    }
    vibesync_engine::scanner::clear_scan_skips();
    let scan = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    assert_eq!(scan.len(), 1, "the readable file is still scanned");
    let skips = vibesync_engine::scanner::scan_skips();
    a.state.mark_deletions_except("claude/projects", &scan, &skips);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(a.state.files.values().all(|st| !st.deleted_locally));
}

/// A file deleted between the scan and the upload is not an error.
#[test]
fn a_file_vanishing_before_upload_does_not_fail_the_push() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    a.write("v1\n", 1_000);
    let entries = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    std::fs::remove_file(a.session()).unwrap();
    let r = sync::push_with_listing(&entries, &mut a.state, &store, "a", &[]).unwrap();
    assert_eq!((r.pushed, r.failed), (0, 0));
}

/// A corrupt or half-synced sidecar in a folder store used to fail the
/// whole listing — and with it every sync.
#[test]
fn a_corrupt_sidecar_is_skipped_not_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = FolderStore::new(&root);
    let mut a = Machine::new(tmp.path(), "a");
    a.write("v1\n", 1_000);
    write_at(&a.project_dir("dev/proj").join("t.jsonl"), "v1\n", 1_000);
    a.sync(&store);
    std::fs::write(find(&root, "t.jsonl.meta.json"), b"").unwrap();
    let listing = store.list().unwrap();
    assert_eq!(listing.len(), 1);
}

// ------------------------------------------------------- store test doubles

/// Wraps a real store; `put` fails for keys containing any of `refuse`, and
/// every `opens` probe is counted.
struct FlakyStore {
    inner: Box<dyn SyncStore>,
    refuse: std::sync::Mutex<Vec<String>>,
    opens_calls: std::sync::atomic::AtomicUsize,
}

impl FlakyStore {
    fn new(inner: Box<dyn SyncStore>) -> Self {
        FlakyStore { inner, refuse: Default::default(), opens_calls: Default::default() }
    }
    fn refuse(&self, needle: &str) {
        self.refuse.lock().unwrap().push(needle.to_string());
    }
    fn accept_all(&self) {
        self.refuse.lock().unwrap().clear();
    }
    fn opens_calls(&self) -> usize {
        self.opens_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl SyncStore for FlakyStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &vibesync_engine::RemoteMeta) -> anyhow::Result<()> {
        if self.refuse.lock().unwrap().iter().any(|n| logical.contains(n.as_str())) {
            anyhow::bail!("Storage returned HTTP 503 on upload.");
        }
        self.inner.put(logical, plain, meta)
    }
    fn get(&self, logical: &str) -> anyhow::Result<Option<(Vec<u8>, vibesync_engine::RemoteMeta)>> {
        self.inner.get(logical)
    }
    fn list(&self) -> anyhow::Result<Vec<(String, vibesync_engine::RemoteMeta)>> {
        self.inner.list()
    }
    fn key_id(&self) -> Option<String> {
        self.inner.key_id()
    }
    fn opens(&self, logical: &str) -> anyhow::Result<Option<bool>> {
        self.opens_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.opens(logical)
    }
}

fn age_store(root: &Path, pass: &str) -> FolderStore {
    FolderStore::with_codec(root, Box::new(AgeCodec::new(pass)))
}

// ------------------------------------------------------------------ push

/// One upload the store refuses costs that file: the others are recorded,
/// the refused one keeps its old state and goes up on the next sync.
#[test]
fn a_refused_upload_costs_that_file_and_is_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FlakyStore::new(Box::new(FolderStore::new(tmp.path().join("store"))));
    let mut a = Machine::new(tmp.path(), "a");
    a.write("ok\n", 1_000);
    write_at(&a.project_dir("dev/proj").join("refused.jsonl"), "no\n", 1_000);
    store.refuse("refused");

    let entries = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    let r = sync::push_with_listing(&entries, &mut a.state, &store, "a", &[]).unwrap();
    assert_eq!((r.pushed, r.failed), (1, 1));
    assert!(r.last_error.as_deref().unwrap_or("").contains("HTTP 503"));
    assert!(!a.state.files.keys().any(|k| k.contains("refused")), "no state for a failed upload");

    store.accept_all();
    let r = sync::push_with_listing(&entries, &mut a.state, &store, "a", &store.list().unwrap()).unwrap();
    assert_eq!((r.pushed, r.failed), (1, 0));
}

// ------------------------------------------------------------- self-heal

/// The dangerous direction: THIS machine now holds the wrong passphrase.
/// Its earlier uploads (made with the right one) are what it can't open —
/// re-sending them would re-encrypt the fleet's good copies under the wrong
/// key. The fleet check must stop that.
#[test]
fn a_machine_with_the_wrong_passphrase_never_reencrypts_its_good_uploads() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    write_at(&b.project_dir("dev/other").join("b.jsonl"), "from b\n", 1_000);
    b.sync(&age_store(&root, "right"));
    a.write("good\n", 1_000);
    a.sync(&age_store(&root, "right"));

    // A's passphrase is changed to a typo.
    let wrong = age_store(&root, "typo");
    let check = sync::stale_own_uploads(&wrong, &wrong.list().unwrap(), &|s| s == "a").unwrap();
    assert!(!check.key_matches_fleet());
    assert!(check.stale.is_empty());
    let r = a.sync(&wrong);
    assert_eq!(r.pushed, 0, "nothing re-uploaded under the wrong key");

    // The fleet still reads A's object.
    let mut c = Machine::new(tmp.path(), "c");
    let r = c.sync(&age_store(&root, "right"));
    assert_eq!(r.failed, 0);
}

/// A fleet of one that deliberately changes its passphrase re-encrypts its
/// own objects: there is no other machine to compare against.
#[test]
fn a_single_machine_changing_its_passphrase_reencrypts_its_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut a = Machine::new(tmp.path(), "a");
    a.write("mine\n", 1_000);
    a.sync(&age_store(&root, "old"));
    let new = age_store(&root, "new");
    assert_eq!(a.sync(&new).pushed, 1);
    let key = new.list().unwrap()[0].0.clone();
    assert!(new.get(&key).is_ok());
}

#[test]
fn a_tie_between_readable_and_unreadable_fleet_objects_is_not_a_match() {
    let tie = sync::OwnUploadCheck { stale: vec![], others_readable: 3, others_unreadable: 3 };
    assert!(!tie.key_matches_fleet());
    let alone = sync::OwnUploadCheck::default();
    assert!(alone.key_matches_fleet());
    let most = sync::OwnUploadCheck { stale: vec![], others_readable: 4, others_unreadable: 1 };
    assert!(most.key_matches_fleet());
}

/// The header check runs once per key, not every sync — on a real bucket
/// it is one request per object this machine ever uploaded.
#[test]
fn the_self_heal_check_runs_once_per_passphrase() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut a = Machine::new(tmp.path(), "a");
    a.write("x\n", 1_000);
    let store = FlakyStore::new(Box::new(age_store(&root, "p1")));
    a.sync(&store);
    assert!(a.state.own_checked_key.is_some(), "checked (an empty store) on the first sync");
    let first = store.opens_calls();
    a.write("x\ny\n", 2_000);
    a.sync(&store);
    a.sync(&store);
    assert_eq!(store.opens_calls(), first, "same key: no second check");

    let store2 = FlakyStore::new(Box::new(age_store(&root, "p2")));
    a.sync(&store2);
    assert!(store2.opens_calls() > 0, "a new key re-checks");
}

/// A re-upload that fails keeps its mark: the pull in the same sync must
/// not record the store's (unreadable) copy as in sync.
#[test]
fn a_failed_reupload_stays_queued_until_it_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut a = Machine::new(tmp.path(), "a");
    a.write("hello\n", 1_000);
    a.sync(&age_store(&root, "typo"));

    let flaky = FlakyStore::new(Box::new(age_store(&root, "right")));
    flaky.refuse("s.jsonl");
    let r = a.sync(&flaky);
    assert_eq!((r.pushed, r.failed), (0, 1));
    assert!(a.state.files.values().any(|st| st.hash == vibesync_engine::state::REUPLOAD));

    flaky.accept_all();
    assert_eq!(a.sync(&flaky).pushed, 1);
    let mut b = Machine::new(tmp.path(), "b");
    assert_eq!(b.sync(&age_store(&root, "right")).failed, 0);
    assert_eq!(b.read(), "hello\n");
}

#[test]
fn queueing_a_reupload_never_overrides_a_recorded_deletion() {
    let mut state = SyncState::default();
    state.files.insert(
        "claude/projects/p/gone.jsonl".into(),
        vibesync_engine::FileState { hash: "h".into(), mtime_ms: 1, size: 1, deleted_locally: true },
    );
    state.files.insert(
        "claude/projects/p/here.jsonl".into(),
        vibesync_engine::FileState { hash: "h".into(), mtime_ms: 1, size: 1, deleted_locally: false },
    );
    let keys = vec![
        "claude/projects/p/gone.jsonl".to_string(),
        "claude/projects/p/here.jsonl".to_string(),
        "meta/untracked.json".to_string(),
    ];
    assert_eq!(sync::mark_for_reupload(&mut state, &keys), 2);
    assert!(state.files["claude/projects/p/gone.jsonl"].deleted_locally);
    assert_eq!(state.files["claude/projects/p/gone.jsonl"].hash, "h");
    assert_eq!(state.files["meta/untracked.json"].hash, vibesync_engine::state::REUPLOAD);
    // Idempotent.
    assert_eq!(sync::mark_for_reupload(&mut state, &keys), 0);
}

#[test]
fn an_unencrypted_store_has_nothing_to_self_heal() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    a.write("x\n", 1_000);
    a.sync(&store);
    assert_eq!(store.key_id(), None);
    assert_eq!(a.state.own_checked_key, None);
    let check = sync::stale_own_uploads(&store, &store.list().unwrap(), &|s| s == "a").unwrap();
    assert!(check.stale.is_empty());
}

// ------------------------------------------------------- deletion edges

/// The (file name, content) tombstone is narrow: same content under a
/// different name, or the same name with new content, still arrives.
#[test]
fn tombstones_only_match_the_same_file_with_the_same_content() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("shared text\n", 1_000);
    a.sync(&store);
    b.sync(&store);
    std::fs::remove_file(b.session()).unwrap();
    let scan = CLAUDE_CODE.scan(&b.home, &b.tok, false).unwrap();
    b.state.mark_deletions("claude/projects", &scan);

    // Different name, same content, other project.
    write_at(&a.project_dir("dev/other").join("t.jsonl"), "shared text\n", 1_000);
    // Same name, NEW content, other project (a continued session).
    write_at(&a.project_dir("dev/third").join("s.jsonl"), "shared text\nmore\n", 2_000);
    a.sync(&store);

    let r = b.sync(&store);
    assert_eq!(r.pulled, 2);
    assert!(b.project_dir("dev/other").join("t.jsonl").exists());
    assert!(b.project_dir("dev/third").join("s.jsonl").exists());
    assert!(!b.session().exists(), "the deleted one itself stays deleted");
}

/// Marked deleted by mistake, then edited locally: the local edit is newer,
/// so it is uploaded — never overwritten by the older store copy.
#[test]
fn a_mistakenly_marked_file_edited_locally_wins_over_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "a");
    let mut b = Machine::new(tmp.path(), "b");
    a.write("v1\n", 1_000);
    a.sync(&store);
    b.sync(&store);
    b.state.mark_deletions("claude/projects", &[]);
    b.write("v1\nb-edit\n", 5_000);
    let r = b.sync(&store);
    assert_eq!(r.pushed, 1);
    assert_eq!(b.read(), "v1\nb-edit\n");
    a.sync(&store);
    assert_eq!(a.read(), "v1\nb-edit\n");
}

// ---------------------------------------------------------------- backups

#[test]
fn backups_are_numbered_and_identical_ones_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x.jsonl");
    std::fs::write(&f, "one").unwrap();
    let b1 = sync::backup_sibling(&f).unwrap();
    assert_eq!(b1.file_name().unwrap(), "x.jsonl.vibesync-bak");
    assert_eq!(sync::backup_sibling(&f).unwrap(), b1, "same bytes: reused");
    std::fs::write(&f, "two").unwrap();
    let b2 = sync::backup_sibling(&f).unwrap();
    assert_eq!(b2.file_name().unwrap(), "x.jsonl.2.vibesync-bak");
    std::fs::write(&f, "one").unwrap();
    assert_eq!(sync::backup_sibling(&f).unwrap(), b1, "matches an older backup");
    assert_eq!(std::fs::read_to_string(&b1).unwrap(), "one");
    assert_eq!(std::fs::read_to_string(&b2).unwrap(), "two");

    let bare = tmp.path().join("LICENSE");
    std::fs::write(&bare, "l").unwrap();
    assert_eq!(sync::backup_sibling(&bare).unwrap().file_name().unwrap(), "LICENSE.vibesync-bak");
}

/// Numbered backups must stay invisible to the scanner (they end in
/// `.vibesync-bak`), or they would be uploaded as sessions.
#[test]
fn numbered_backups_are_never_scanned() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    a.write("v\n", 1_000);
    let dir = a.session().parent().unwrap().to_path_buf();
    std::fs::write(dir.join("s.jsonl.vibesync-bak"), "old").unwrap();
    std::fs::write(dir.join("s.jsonl.2.vibesync-bak"), "older").unwrap();
    let scan = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    assert_eq!(scan.len(), 1);
}

// ---------------------------------------------------------------- scanning

/// Settings files are scanned as single-file roots; an unreadable one is
/// skipped and remembered, not fatal.
#[cfg(unix)]
#[test]
fn an_unreadable_single_file_root_is_skipped_and_remembered() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    let settings = a.home.join(".claude/settings.json");
    write_at(&settings, "{\"fresh\":1}", 1_000);
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&settings).is_ok() {
        return; // root
    }
    vibesync_engine::scanner::clear_scan_skips();
    let scan = CLAUDE_CODE.scan(&a.home, &a.tok, false).unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!scan.iter().any(|e| e.logical.ends_with("settings.json")));
    assert!(vibesync_engine::scanner::scan_skips().iter().any(|k| k.ends_with("settings.json")));
}

/// Profiles (`~/.claude-work`) report unreadable paths in the same key
/// space as their state entries.
#[cfg(unix)]
#[test]
fn unreadable_paths_in_a_profile_are_keyed_like_its_state() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "a");
    let locked = a.home.join(".claude-work/projects/p");
    write_at(&locked.join("s.jsonl"), "x\n", 1_000);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    vibesync_engine::scanner::clear_scan_skips();
    CLAUDE_CODE.scan_dir(&a.home, ".claude-work", &a.tok, false).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let skips = vibesync_engine::scanner::scan_skips();
    assert!(skips.iter().any(|k| k.starts_with("claude/profiles/.claude-work/projects/")), "{skips:?}");
}

/// End to end with the real-world failure from debug.log: this machine's
/// copy of `meta/project_machines.json` was written under its old
/// passphrase. The self-heal check must queue it, so the next sync's
/// project-table step (reupload_pending) replaces it.
#[test]
fn the_self_heal_check_queues_this_machines_unreadable_project_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let map = {
        let mut m = vibesync_engine::gitmap::GitMap::default();
        m.roots.insert("github.com/o/r".into(), "/Users/me/dev/r".into());
        m
    };
    vibesync_engine::atlas::sync_machines(&age_store(&root, "old"), &map, "/Users/me", "a");
    // Another machine's object, readable under the corrected passphrase.
    let mut b = Machine::new(tmp.path(), "b");
    b.write("from b\n", 1_000);
    b.sync(&age_store(&root, "right"));

    let right = age_store(&root, "right");
    let check = sync::stale_own_uploads(&right, &right.list().unwrap(), &|s| s == "a").unwrap();
    assert!(check.key_matches_fleet());
    assert_eq!(check.stale, vec![vibesync_engine::atlas::MACHINES_KEY.to_string()]);
    let mut state = SyncState::default();
    sync::mark_for_reupload(&mut state, &check.stale);
    assert!(state.reupload_pending(vibesync_engine::atlas::MACHINES_KEY));
    let (_, healed) =
        vibesync_engine::atlas::sync_machines_healing(&right, &map, "/Users/me", "a", true);
    assert!(healed);
}

/// A symlink planted at a backup name is never written through.
#[cfg(unix)]
#[test]
fn a_symlink_at_a_backup_name_is_refused_and_the_original_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x.jsonl");
    std::fs::write(&f, "precious").unwrap();
    let target = tmp.path().join("elsewhere");
    std::fs::write(&target, "do not touch").unwrap();
    std::os::unix::fs::symlink(&target, tmp.path().join("x.jsonl.vibesync-bak")).unwrap();
    assert!(sync::backup_sibling(&f).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "do not touch");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "precious");
}

/// File names are not always UTF-8 on Unix; the backup must sit beside the
/// real file, not beside a lossy look-alike name.
#[cfg(target_os = "linux")]
#[test]
fn a_non_utf8_file_name_gets_a_matching_backup_name() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.jsonl");
    let f = tmp.path().join(name);
    std::fs::write(&f, "x").unwrap();
    let bak = sync::backup_sibling(&f).unwrap();
    let mut want = name.to_os_string();
    want.push(".vibesync-bak");
    assert_eq!(bak.file_name().unwrap(), want.as_os_str());
}
