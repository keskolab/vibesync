//! Two-machine roundtrip: push from "machine A", pull on "machine B",
//! verify remapped paths, retention (no resurrection), and newer-local wins.

use std::path::{Path, PathBuf};

use vibesync_engine::adapters::CLAUDE_CODE;
use vibesync_engine::tokenizer::encode_cwd;
use vibesync_engine::{sync, FolderStore, SyncState, Tokenizer};

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

    /// Create a transcript in a project dir named for a cwd under this home.
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

    fn scan(&self) -> Vec<vibesync_engine::FileEntry> {
        CLAUDE_CODE.scan(&self.home, &self.tok, false).unwrap()
    }
}

#[test]
fn two_machine_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "machine_a");
    let mut b = Machine::new(tmp.path(), "machine_b");

    a.write_session("dev/proj", "11111111-aaaa", "{\"line\":1}\n{\"line\":2}\n");
    a.write_session("dev/other", "22222222-bbbb", "{\"other\":true}\n");

    // Push from A.
    let entries = a.scan();
    assert_eq!(entries.len(), 2);
    assert!(
        entries.iter().all(|e| e.logical.starts_with("projects/${EHOME}-dev-")),
        "encoded home must be tokenized: {:?}",
        entries.iter().map(|e| &e.logical).collect::<Vec<_>>()
    );
    let report = sync::push(&entries, &mut a.state, &store, "machine-a").unwrap();
    assert_eq!(report.pushed, 2);

    // Second push is a no-op.
    let report = sync::push(&a.scan(), &mut a.state, &store, "machine-a").unwrap();
    assert_eq!(report.pushed, 0);
    assert_eq!(report.unchanged, 2);

    // Pull on B: files land under B's home with B's encoded paths.
    let report = sync::pull(&CLAUDE_CODE, &b.home, &b.tok, &mut b.state, &store, false).unwrap();
    assert_eq!(report.pulled, 2);
    let restored = b.session_path("dev/proj", "11111111-aaaa");
    assert!(restored.exists(), "expected {}", restored.display());
    assert_eq!(
        std::fs::read_to_string(&restored).unwrap(),
        "{\"line\":1}\n{\"line\":2}\n"
    );

    // Pull again: nothing to do.
    let report = sync::pull(&CLAUDE_CODE, &b.home, &b.tok, &mut b.state, &store, false).unwrap();
    assert_eq!(report.pulled, 0);
    assert_eq!(report.unchanged, 2);
}

#[test]
fn deleted_sessions_are_not_resurrected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "machine_a");

    a.write_session("dev/proj", "11111111-aaaa", "content\n");
    sync::push(&a.scan(), &mut a.state, &store, "a").unwrap();

    // Simulate Claude Code's retention cleanup deleting the transcript.
    std::fs::remove_file(a.session_path("dev/proj", "11111111-aaaa")).unwrap();
    let marked = a.state.mark_deletions("projects", &a.scan());
    assert_eq!(marked, 1);

    // Pull must NOT bring it back.
    let report = sync::pull(&CLAUDE_CODE, &a.home, &a.tok, &mut a.state, &store, false).unwrap();
    assert_eq!(report.pulled, 0);
    assert_eq!(report.skipped_deleted, 1);
    assert!(!a.session_path("dev/proj", "11111111-aaaa").exists());
}

#[test]
fn newer_local_content_is_never_clobbered() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut a = Machine::new(tmp.path(), "machine_a");
    let mut b = Machine::new(tmp.path(), "machine_b");

    // A pushes v1 with an old mtime.
    let path_a = a.write_session("dev/proj", "11111111-aaaa", "old remote\n");
    filetime::set_file_mtime(&path_a, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
    sync::push(&a.scan(), &mut a.state, &store, "a").unwrap();

    // B already has newer local content for the same session.
    let path_b = b.write_session("dev/proj", "11111111-aaaa", "newer local\n");
    filetime::set_file_mtime(&path_b, filetime::FileTime::from_unix_time(2_000_000, 0)).unwrap();

    let report = sync::pull(&CLAUDE_CODE, &b.home, &b.tok, &mut b.state, &store, false).unwrap();
    assert_eq!(report.skipped_newer_local, 1);
    assert_eq!(std::fs::read_to_string(&path_b).unwrap(), "newer local\n");

    // Reverse case: B's file is older than the store's -> it is replaced, with a backup kept.
    filetime::set_file_mtime(&path_b, filetime::FileTime::from_unix_time(500_000, 0)).unwrap();
    let report = sync::pull(&CLAUDE_CODE, &b.home, &b.tok, &mut b.state, &store, false).unwrap();
    assert_eq!(report.pulled, 1);
    assert_eq!(std::fs::read_to_string(&path_b).unwrap(), "old remote\n");
    let bak = path_b.with_extension("jsonl.vibesync-bak");
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), "newer local\n");
}

#[test]
fn adapter_detection() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "machine_a");
    assert!(!CLAUDE_CODE.detect(&a.home));
    a.write_session("dev/proj", "11111111-aaaa", "x\n");
    assert!(CLAUDE_CODE.detect(&a.home));
}

#[test]
fn expanded_scopes_and_plugin_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Machine::new(tmp.path(), "machine_a");
    let dot = a.home.join(".claude");
    let w = |rel: &str, data: &str| {
        let p = dot.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("CLAUDE.md", "global instructions");
    w("settings.json", "{}");
    w("history.jsonl", "{}\n");
    w("agents/reviewer.md", "agent");
    w("skills/deploy/SKILL.md", "skill");
    w("rules/style.md", "rule");
    w("tasks/t1.json", "{}");
    w("plugins/installed_plugins.json", "{}");
    w("plugins/cache/huge/blob.bin", "xxxxxxxx");

    let tok = &a.tok;
    let default_scan = CLAUDE_CODE.scan(&a.home, tok, false).unwrap();
    let logicals: Vec<&str> = default_scan.iter().map(|e| e.logical.as_str()).collect();
    for expected in [
        "meta/CLAUDE.md",
        "meta/settings.json",
        "meta/history.jsonl",
        "agents/reviewer.md",
        "skills/deploy/SKILL.md",
        "rules/style.md",
        "tasks/t1.json",
    ] {
        assert!(logicals.contains(&expected), "missing {expected}: {logicals:?}");
    }
    // Plugins never sync by default.
    assert!(!logicals.iter().any(|l| l.starts_with("plugins/")));

    // Opt-in includes the manifest but NEVER the cache.
    let with_plugins = CLAUDE_CODE.scan(&a.home, tok, true).unwrap();
    let logicals: Vec<&str> = with_plugins.iter().map(|e| e.logical.as_str()).collect();
    assert!(logicals.contains(&"plugins/installed_plugins.json"));
    assert!(!logicals.iter().any(|l| l.contains("cache")), "cache leaked: {logicals:?}");

    // File roots resolve back to the right absolute path on another machine.
    let b = Machine::new(tmp.path(), "machine_b");
    let abs = CLAUDE_CODE.resolve("meta/CLAUDE.md", &b.home, &b.tok, false).unwrap();
    assert_eq!(abs, b.home.join(".claude").join("CLAUDE.md"));
}
