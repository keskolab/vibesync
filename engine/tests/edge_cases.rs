//! Edge-case regression suite — every scenario here is either a bug we hit
//! live on the real fleet or an edge we reasoned about while fixing one.
//! Tests run on BOTH OSes in CI: half of this weekend's failures were
//! macOS-green tests meeting Windows paths for the first time.

use std::path::{Path, PathBuf};

use vibesync_engine::atlas;
use vibesync_engine::gitmap::GitMap;
use vibesync_engine::registry;
use vibesync_engine::store::{FolderStore, RemoteMeta, SyncStore};
use vibesync_engine::tokenizer::Tokenizer;
use vibesync_engine::SyncState;

// ---------------------------------------------------------------- helpers

/// A store whose reads fail — models a transient network error.
struct FailingReads {
    puts: std::sync::Mutex<Vec<String>>,
}

impl SyncStore for FailingReads {
    fn put(&self, logical: &str, _plain: &[u8], _meta: &RemoteMeta) -> anyhow::Result<()> {
        self.puts.lock().unwrap().push(logical.to_string());
        Ok(())
    }
    fn get(&self, _logical: &str) -> anyhow::Result<Option<(Vec<u8>, RemoteMeta)>> {
        anyhow::bail!("transient network error")
    }
    fn list(&self) -> anyhow::Result<Vec<(String, RemoteMeta)>> {
        anyhow::bail!("transient network error")
    }
}

fn make_repo(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(
        dir.join(".git/config"),
        format!("[remote \"origin\"]\n\turl = {origin}\n"),
    )
    .unwrap();
}

fn make_codex_db(home: &Path, name: &str) -> PathBuf {
    let p = home.join(".codex").join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute_batch(
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
    p
}

// ---------------------------------------------------------------- atlas

/// Review finding, fixed live: a transient fetch failure must not make a
/// machine republish a local-only atlas over the fleet union.
#[test]
fn atlas_fetch_failure_never_clobbers_the_fleet() {
    let store = FailingReads { puts: Default::default() };
    let mut map = GitMap::default();
    map.roots.insert("github.com/o/r".into(), "/Users/u/dev/r".into());
    let atlas = atlas::sync_atlas(&store, &map, "/Users/u", "m");
    // Local knowledge is still usable this sync...
    assert_eq!(atlas["github.com/o/r"], vec!["${HOME}/dev/r".to_string()]);
    // ...but nothing was published over the unreadable fleet copy.
    assert!(store.puts.lock().unwrap().is_empty());
}

/// A corrupt store copy is a fresh start, not a fatal error.
#[test]
fn atlas_corrupt_store_copy_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    store
        .put(
            atlas::KEY,
            b"definitely not json",
            &RemoteMeta { hash: "x".into(), mtime_ms: 0, size: 3, source: "m".into() },
        )
        .unwrap();
    let mut map = GitMap::default();
    map.roots.insert("github.com/o/r".into(), "/Users/u/dev/r".into());
    let atlas = atlas::sync_atlas(&store, &map, "/Users/u", "m");
    assert_eq!(atlas["github.com/o/r"], vec!["${HOME}/dev/r".to_string()]);
    // The rebuilt atlas replaced the corrupt copy.
    let (bytes, _) = store.get(atlas::KEY).unwrap().unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
}

/// Windows drive letters vary in case in real fleet data (observed live);
/// the home-relative form must still be produced.
#[test]
fn atlas_home_strip_tolerates_drive_case() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let mut map = GitMap::default();
    map.roots.insert("github.com/o/r".into(), "c:\\Users\\w\\dev\\r".into());
    let atlas = atlas::sync_atlas(&store, &map, "C:\\Users\\w", "m");
    assert_eq!(atlas["github.com/o/r"], vec!["${HOME}\\dev\\r".to_string()]);
}

// ---------------------------------------------------------------- gitmap

/// Live catch: Codex records cwds in extended-length form; roots learned
/// from them must not keep the \\?\ prefix (they'd never prefix-match).
#[cfg(windows)]
#[test]
fn learned_roots_strip_extended_length_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    make_repo(&repo, "git@github.com:o/ext.git");
    let mut map = GitMap::default();
    let ext = PathBuf::from(format!("\\\\?\\{}", repo.to_string_lossy()));
    assert!(map.learn(&ext));
    let root = &map.roots["github.com/o/ext"];
    assert!(!root.starts_with("\\\\?\\"), "{root}");
}

/// Same repo cloned twice on one machine (live catch): the second clone
/// tokenizes to the shared identity, expansion targets the primary.
#[test]
fn tokenizer_second_clone_alias_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    make_repo(&first, "git@github.com:o/two.git");
    make_repo(&second, "git@github.com:o/two.git");
    let mut map = GitMap::default();
    assert!(map.learn(&first));
    assert!(map.learn(&second));
    let tok = Tokenizer::with_case_sensitivity(&tmp.path().to_string_lossy(), cfg!(windows))
        .with_gitmap(&map);
    let t2 = tok.tokenize_plain(&second.to_string_lossy());
    let t1 = tok.tokenize_plain(&first.to_string_lossy());
    assert_eq!(t1, t2);
    assert_eq!(Path::new(&tok.expand_plain(&t2)), first.as_path());
}

// ---------------------------------------------------------------- codex

/// The thread db is generation-named; a future state_6 must win.
#[test]
fn codex_state_db_prefers_highest_generation() {
    let tmp = tempfile::tempdir().unwrap();
    make_codex_db(tmp.path(), "state_5.sqlite");
    make_codex_db(tmp.path(), "state_7.sqlite");
    let picked = vibesync_engine::codex::state_db(tmp.path()).unwrap();
    assert!(picked.to_string_lossy().ends_with("state_7.sqlite"), "{picked:?}");
}

/// An unparseable sandbox_policy must ride through push+apply verbatim —
/// schema drift can never fail the whole merge.
#[test]
fn codex_invalid_sandbox_policy_passes_through() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let db_a = make_codex_db(&a, "state_5.sqlite");
    let a_home = a.to_string_lossy().into_owned();
    rusqlite::Connection::open(&db_a)
        .unwrap()
        .execute(
            "INSERT INTO threads VALUES ('tx', ?1, 100, 200, 's', 'p', ?2, 'T',
               'not-json{', 'on-request', 100000, 200000, 200000, NULL)",
            rusqlite::params![
                vibesync_engine::codex::state_db(&a).unwrap().to_string_lossy().into_owned(),
                a.join("proj").to_string_lossy().into_owned(),
            ],
        )
        .unwrap();
    let tok_a = Tokenizer::with_case_sensitivity(&a_home, cfg!(windows));
    let mut st = SyncState::default();
    assert_eq!(
        vibesync_engine::codex::db_push(&a, &tok_a, &mut st, &store, "a", &[]).unwrap(),
        1
    );
    let (bytes, _) = store.get("codex/db/tx.json").unwrap().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["thread"]["sandbox_policy"], "not-json{");
}

/// A rollout containing a non-UTF-8 / non-JSON line must pass through
/// byte-for-byte while path lines still tokenize.
#[test]
fn codex_rollout_binary_lines_survive_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let a = tmp.path().join("a");
    let a_home = a.to_string_lossy().into_owned();
    let dir = a.join(".codex/sessions/2026/07/13");
    std::fs::create_dir_all(&dir).unwrap();
    let meta_line = serde_json::json!({
        "timestamp": "t", "type": "session_meta",
        "payload": {"id": "rb", "cwd": format!("{a_home}/proj")},
    })
    .to_string();
    let mut content = Vec::new();
    content.extend_from_slice(meta_line.as_bytes());
    content.push(b'\n');
    content.extend_from_slice(&[0xFF, 0xFE, 0x00, b'b', b'i', b'n']); // invalid UTF-8
    content.push(b'\n');
    std::fs::write(dir.join("rollout-rb.jsonl"), &content).unwrap();
    let tok = Tokenizer::with_case_sensitivity(&a_home, cfg!(windows));
    let mut st = SyncState::default();
    assert_eq!(
        vibesync_engine::codex::push_sessions(&a, &tok, &mut st, &store, "a", &[]).unwrap(),
        1
    );
    let (bytes, _) =
        store.get("codex/sessions/2026/07/13/rollout-rb.jsonl").unwrap().unwrap();
    assert!(bytes.windows(6).any(|w| w == [0xFF, 0xFE, 0x00, b'b', b'i', b'n']));
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("${HOME}/proj"), "{text}");
}

/// Live catch (the infinite Mac<->Windows re-upload): a store object with
/// foreign-native token tails converges in ONE canonicalizing push, then
/// stays settled.
#[test]
fn codex_foreign_tail_rollout_converges_in_one_push() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let b = tmp.path().join("b");
    let b_home = b.to_string_lossy().into_owned();
    // A foreign machine (other OS, old build) pushed backslash token tails.
    let stale = format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "t", "type": "session_meta",
            "payload": {"id": "rf", "cwd": "${HOME}\\proj"},
        }),
    );
    store
        .put(
            "codex/sessions/2026/07/13/rollout-rf.jsonl",
            stale.as_bytes(),
            &RemoteMeta {
                hash: vibesync_engine::scanner::hash_bytes(stale.as_bytes()),
                mtime_ms: 111,
                size: stale.len() as u64,
                source: "w".into(),
            },
        )
        .unwrap();
    let tok = Tokenizer::with_case_sensitivity(&b_home, cfg!(windows));
    let mut st = SyncState::default();
    let listing = store.list().unwrap();
    vibesync_engine::codex::apply(&b, &tok, &mut st, &store, &listing, &|| {}, &|_| {})
        .unwrap();
    let local = std::fs::read_to_string(b.join(".codex/sessions/2026/07/13/rollout-rf.jsonl"))
        .unwrap();
    assert!(!local.contains("${HOME}"), "{local}");
    // One canonicalizing push (store had non-canonical tails)...
    let n1 = vibesync_engine::codex::push_sessions(&b, &tok, &mut st, &store, "b", &store.list().unwrap())
        .unwrap();
    // ...then settled forever.
    let n2 = vibesync_engine::codex::push_sessions(&b, &tok, &mut st, &store, "b", &store.list().unwrap())
        .unwrap();
    assert!(n1 <= 1);
    assert_eq!(n2, 0);
    let (bytes, _) =
        store.get("codex/sessions/2026/07/13/rollout-rf.jsonl").unwrap().unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("${HOME}/proj"));
}

// ---------------------------------------------------------------- vscode

/// Live catch: VS Code flushes its in-memory chat index over external
/// merges. The reconcile pass must restore clobbered entries on the next
/// sync even though the files already count as applied.
#[test]
fn vscode_index_reconcile_restores_clobbered_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let home = tmp.path().join("home");
    let tok = Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows));

    // A workspace with one native session already indexed.
    let root = tmp.path().join("ws_root");
    let ws = root.join("aaaa0000");
    std::fs::create_dir_all(ws.join("chatSessions")).unwrap();
    std::fs::write(
        ws.join("workspace.json"),
        format!(
            "{{\"folder\": \"file://{}/proj\"}}",
            home.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
        conn.execute(
            "INSERT INTO ItemTable VALUES ('chat.ChatSessionStore.index',
             '{\"version\":1,\"entries\":{}}')",
            [],
        )
        .unwrap();
    }
    // A synced session arrives.
    let session = "{\"chat\":1}\n";
    store
        .put(
            "vscode/ws/${HOME}/proj/chatSessions/synced-1.jsonl",
            session.as_bytes(),
            &RemoteMeta {
                hash: vibesync_engine::scanner::hash_bytes(session.as_bytes()),
                mtime_ms: 123,
                size: session.len() as u64,
                source: "a".into(),
            },
        )
        .unwrap();
    let mut st = SyncState::default();
    let r = vibesync_engine::vscode::apply_roots(&[root.clone()], &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 1);
    let read_index = || -> serde_json::Value {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key='chat.ChatSessionStore.index'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&v).unwrap()
    };
    assert!(read_index()["entries"]["synced-1"].is_object());

    // VS Code clobbers the index from its own memory.
    {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        conn.execute(
            "UPDATE ItemTable SET value='{\"version\":1,\"entries\":{}}'
             WHERE key='chat.ChatSessionStore.index'",
            [],
        )
        .unwrap();
    }
    // Next sync: nothing new to apply, but the reconcile restores the entry.
    let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 0);
    assert!(
        read_index()["entries"]["synced-1"].is_object(),
        "reconcile must re-add clobbered entries"
    );
}

/// Live catch (VS Code Windows verification): a running VS Code holds
/// state.vscdb, so the index merge hits SQLITE_BUSY. The apply must still
/// place the session files without erroring, and the index must converge
/// on the next sync once the db is free again.
#[test]
fn vscode_locked_index_converges_after_release() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let home = tmp.path().join("home");
    let tok = Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows));

    let root = tmp.path().join("ws_root");
    let ws = root.join("bbbb0000");
    std::fs::create_dir_all(ws.join("chatSessions")).unwrap();
    std::fs::write(
        ws.join("workspace.json"),
        format!(
            "{{\"folder\": \"file://{}/proj\"}}",
            home.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
    }
    let session = "{\"chat\":1}\n";
    store
        .put(
            "vscode/ws/${HOME}/proj/chatSessions/locked-1.jsonl",
            session.as_bytes(),
            &RemoteMeta {
                hash: vibesync_engine::scanner::hash_bytes(session.as_bytes()),
                mtime_ms: 123,
                size: session.len() as u64,
                source: "a".into(),
            },
        )
        .unwrap();

    // "VS Code is running": hold the db exclusively through the whole sync.
    let lock = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let mut st = SyncState::default();
    let r = vibesync_engine::vscode::apply_roots(&[root.clone()], &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 1, "files must land even when the index db is locked");
    assert!(ws.join("chatSessions/locked-1.jsonl").exists());
    drop(lock);

    // Next sync with the db free: nothing new to pull, index converges.
    let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 0);
    let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key='chat.ChatSessionStore.index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let idx: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert!(
        idx["entries"]["locked-1"].is_object(),
        "index must converge on the sync after the lock clears: {idx}"
    );
}

/// Live catch: Windows workspace URIs decode with a lowercase drive letter
/// (`file:///c%3A/Temp/repo`) while the gitmap may have learned the root
/// with an uppercase one. Both must produce the identical sanitized
/// ${GIT} store key, or every cross-machine session parks forever.
#[test]
fn vscode_drive_case_still_maps_to_git_identity_key() {
    let mut map = vibesync_engine::gitmap::GitMap::default();
    map.roots
        .insert("github.com/johnkesko/test-repo".into(), "C:\\Temp\\test-repo".into());
    // Windows tokenizers are case-insensitive; model that regardless of host.
    let tok = Tokenizer::with_case_sensitivity("C:\\Users\\you", true).with_gitmap(&map);

    // The native shape scan/apply derive from the decoded URI — note the
    // lowercase drive letter (URI decoding always yields one).
    let key = tok
        .tokenize_plain("c:\\Temp\\test-repo")
        .replace('\\', "/")
        .replace(':', "%3A");
    assert_eq!(key, "${GIT%3Agithub.com%3Ajohnkesko%3Atest-repo}");

    // A Mac clone of the same repo must produce the same key.
    let mut mac_map = vibesync_engine::gitmap::GitMap::default();
    mac_map
        .roots
        .insert("github.com/johnkesko/test-repo".into(), "/Users/anna/dev/test-repo".into());
    let mac_tok = Tokenizer::with_case_sensitivity("/Users/anna", false).with_gitmap(&mac_map);
    let mac_key = mac_tok
        .tokenize_plain("/Users/anna/dev/test-repo")
        .replace('\\', "/")
        .replace(':', "%3A");
    assert_eq!(mac_key, key, "both OSes must key the workspace identically");
}

/// The panel entry's title is how the user recognizes a synced chat.
/// A session with a customTitle must surface it; a title-less session
/// must fall back to a placeholder instead of failing the merge.
#[test]
fn vscode_index_titles_come_from_session_content() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FolderStore::new(tmp.path().join("store"));
    let home = tmp.path().join("home");
    let tok = Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows));

    let root = tmp.path().join("ws_root");
    let ws = root.join("cccc0000");
    std::fs::create_dir_all(ws.join("chatSessions")).unwrap();
    std::fs::write(
        ws.join("workspace.json"),
        format!(
            "{{\"folder\": \"file://{}/proj\"}}",
            home.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
    }
    let titled = "{\"kind\":0,\"v\":{\"version\":3,\"customTitle\":\"Test message from ComputerA\",\"requests\":[{}]}}\n";
    let untitled = "{\"kind\":0,\"v\":{\"version\":3,\"requests\":[]}}\n";
    for (name, body) in [("titled-1", titled), ("untitled-1", untitled)] {
        store
            .put(
                &format!("vscode/ws/${{HOME}}/proj/chatSessions/{name}.jsonl"),
                body.as_bytes(),
                &RemoteMeta {
                    hash: vibesync_engine::scanner::hash_bytes(body.as_bytes()),
                    mtime_ms: 123,
                    size: body.len() as u64,
                    source: "a".into(),
                },
            )
            .unwrap();
    }
    let mut st = SyncState::default();
    let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 2);
    let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key='chat.ChatSessionStore.index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let idx: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert_eq!(idx["entries"]["titled-1"]["title"], "Test message from ComputerA");
    assert_eq!(idx["entries"]["untitled-1"]["title"], "Synced chat");
}

// ---------------------------------------------------------------- plugins

/// Live catch (plugins opt-in, 2026-07-14): installed_plugins.json and
/// known_marketplaces.json carry absolute installPaths into this machine's
/// plugins/cache/ — machine-local by nature. Synced to Windows they showed
/// phantom Mac installs, and a plugin operation there could prune + clobber
/// the origin's registry. They must neither push nor apply; atomic-write
/// temp residue (blocklist.json.<hex>.tmp) must not travel either.
#[test]
fn plugin_machine_local_registries_never_travel() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let plugins = home.join(".claude/plugins");
    std::fs::create_dir_all(plugins.join("marketplaces/official")).unwrap();
    std::fs::create_dir_all(plugins.join("cache/official/x/1.0")).unwrap();
    std::fs::write(plugins.join("blocklist.json"), "{}").unwrap();
    std::fs::write(plugins.join("installed_plugins.json"), "{\"plugins\":{}}").unwrap();
    std::fs::write(plugins.join("known_marketplaces.json"), "{}").unwrap();
    std::fs::write(plugins.join(".last_inuse_sweep"), "1").unwrap();
    std::fs::write(plugins.join("blocklist.json.817da9dc.tmp"), "{}").unwrap();
    std::fs::write(plugins.join("marketplaces/official/manifest.json"), "{}").unwrap();
    std::fs::write(plugins.join("cache/official/x/1.0/huge.bin"), "x").unwrap();

    let tok = Tokenizer::with_case_sensitivity(&home.to_string_lossy(), cfg!(windows));
    let entries =
        vibesync_engine::adapters::CLAUDE_CODE.scan(home, &tok, true).unwrap();
    let logicals: Vec<&str> = entries.iter().map(|e| e.logical.as_str()).collect();
    assert!(logicals.contains(&"claude/plugins/blocklist.json"), "{logicals:?}");
    assert!(
        logicals.contains(&"claude/plugins/marketplaces/official/manifest.json"),
        "{logicals:?}"
    );
    for banned in [
        "installed_plugins.json",
        "known_marketplaces.json",
        ".last_inuse_sweep",
        ".tmp",
        "cache/",
    ] {
        assert!(
            !logicals.iter().any(|l| l.contains(banned)),
            "{banned} must not sync: {logicals:?}"
        );
    }

    // Store residue pushed by older builds must not apply either.
    assert!(vibesync_engine::adapters::CLAUDE_CODE
        .resolve(
            "claude/plugins/installed_plugins.json",
            home,
            &tok,
            true
        )
        .is_none());
    assert!(vibesync_engine::adapters::CLAUDE_CODE
        .resolve("claude/plugins/marketplaces/official/manifest.json", home, &tok, true)
        .is_some());
}

// ---------------------------------------------------------------- registry

/// The sidebar heal's core: entries snap to canonical local paths, and an
/// entry that would end up holding an unexpandable token is left untouched.
#[test]
fn registry_canonicalize_snaps_or_reverts() {
    const ID: &str = "github.com/o/repo";
    let mut map = GitMap::default();
    map.roots.insert(ID.into(), "/Users/u/dev/repo".into());
    let mut atlas_map = std::collections::BTreeMap::new();
    atlas_map.insert(ID.to_string(), vec!["${HOME}/elsewhere/repo".to_string()]);
    let tok = Tokenizer::with_case_sensitivity("/Users/u", false)
        .with_gitmap(&map)
        .with_fleet_aliases(&atlas_map);

    // A ghost entry recorded under the OTHER machine's clone path snaps to
    // this machine's clone.
    let mut e = serde_json::json!({
        "sessionId": "local_1", "cliSessionId": "c1",
        "cwd": "/Users/u/elsewhere/repo",
    });
    assert!(registry::canonicalize_entry(&mut e, &tok));
    assert_eq!(e["cwd"], "/Users/u/dev/repo");

    // A cwd for a repo this machine doesn't have must not be rewritten into
    // a token.
    let mut atlas2 = std::collections::BTreeMap::new();
    atlas2.insert("github.com/o/absent".to_string(), vec!["${HOME}/x/absent".to_string()]);
    let tok2 = Tokenizer::with_case_sensitivity("/Users/u", false).with_fleet_aliases(&atlas2);
    let mut e2 = serde_json::json!({
        "sessionId": "local_2", "cliSessionId": "c2",
        "cwd": "/Users/u/x/absent",
    });
    assert!(!registry::canonicalize_entry(&mut e2, &tok2));
    assert_eq!(e2["cwd"], "/Users/u/x/absent");
    assert!(registry::has_unresolved_paths(&serde_json::json!({
        "cwd": "${GIT:github.com:o:absent}"
    })));
}

// ------------------------------------------- cross-machine layout scenarios
//
// The shape these all share: ONE project, absolute paths that have nothing in
// common across machines, and (mostly) not under `$HOME` — so `${HOME}` /
// `${EHOME}` cannot unify them. Either the git origin does it automatically,
// or a project name does it by hand. Written as string-level tokenizer tests
// so the Windows cases run identically on both CI OSes.

/// One machine's view: its home, its case rules, and where it keeps the repo.
fn machine(home: &str, windows: bool, id: &str, root: &str) -> Tokenizer {
    let mut map = GitMap::default();
    map.roots.insert(id.to_string(), root.to_string());
    Tokenizer::with_case_sensitivity(home, windows).with_gitmap(&map)
}

fn proj_map(name: &str, root: &str) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    m.insert(name.to_string(), root.to_string());
    m
}

const STUFF: &str = "github.com/o/stuff";
const STUFF_TOKEN: &str = "${GIT:github.com:o:stuff}";

/// Scenario A — macOS `/Volumes/Data/stuff` ↔ Windows `D:\misc\stuff`.
/// Different OS, different volume, nothing home-relative on either side.
#[test]
fn scenario_a_mac_volume_and_windows_drive_share_one_key() {
    let mac = machine("/Users/alice", false, STUFF, "/Volumes/Data/stuff");
    let win = machine("C:\\Users\\bob", true, STUFF, "D:\\misc\\stuff");

    let from_mac = mac.tokenize_plain("/Volumes/Data/stuff/notes.md");
    let from_win = win.tokenize_plain("D:\\misc\\stuff\\notes.md");
    assert_eq!(from_mac, format!("{STUFF_TOKEN}/notes.md"));
    assert_eq!(from_mac, from_win, "one project must produce one store key");

    // Each machine expands the shared key back to its OWN location. The
    // Windows tail keeps `/` — harmless for filesystem use, and registry
    // entries get `normalize_separators` (see registry_canonicalize_*).
    assert_eq!(mac.expand_plain(&from_mac), "/Volumes/Data/stuff/notes.md");
    assert_eq!(win.expand_plain(&from_mac), "D:\\misc\\stuff/notes.md");
}

/// Scenario A, same folders but with NO git origin — the README's "plain
/// folder at a random location" case. Nothing matches until a project name
/// is set on both machines.
#[test]
fn scenario_a_plain_folder_needs_a_project_name_on_both_sides() {
    let mac_bare = Tokenizer::with_case_sensitivity("/Users/alice", false);
    let win_bare = Tokenizer::with_case_sensitivity("C:\\Users\\bob", true);
    // Untokenizable: neither path is under its home, so both fall through
    // to the literal path and the project splits in two.
    assert_ne!(
        mac_bare.tokenize_plain("/Volumes/Data/stuff/notes.md"),
        win_bare.tokenize_plain("D:\\misc\\stuff\\notes.md"),
    );

    let mac = mac_bare.with_manual_projects(&proj_map("stuff", "/Volumes/Data/stuff"));
    let win = win_bare.with_manual_projects(&proj_map("stuff", "D:\\misc\\stuff"));
    let from_mac = mac.tokenize_plain("/Volumes/Data/stuff/notes.md");
    let from_win = win.tokenize_plain("D:\\misc\\stuff\\notes.md");
    assert_eq!(from_mac, "${PROJ:stuff}/notes.md");
    assert_eq!(from_mac, from_win);
    assert_eq!(win.expand_plain(&from_mac), "D:\\misc\\stuff/notes.md");
}

/// A mapping on only ONE machine is worse than none: that machine keys
/// `${PROJ:...}` while the other keys `${GIT:...}`, silently splitting one
/// project into two sets of store objects.
#[test]
fn scenario_a_one_sided_project_name_splits_the_project() {
    let mac = machine("/Users/alice", false, STUFF, "/Volumes/Data/stuff")
        .with_manual_projects(&proj_map("stuff", "/Volumes/Data/stuff"));
    let win = machine("C:\\Users\\bob", true, STUFF, "D:\\misc\\stuff");
    assert_eq!(mac.tokenize_plain("/Volumes/Data/stuff/notes.md"), "${PROJ:stuff}/notes.md");
    assert_eq!(win.tokenize_plain("D:\\misc\\stuff\\notes.md"), format!("{STUFF_TOKEN}/notes.md"));
    assert_ne!(
        mac.tokenize_plain("/Volumes/Data/stuff/notes.md"),
        win.tokenize_plain("D:\\misc\\stuff\\notes.md"),
    );
}

/// Scenario B — two macOS machines, both off-home:
/// `/Volumes/Data/Development/stuff` ↔ `/Volumes/Users/username/Development/stuff`.
/// The second path merely LOOKS home-shaped; it is under /Volumes, not /Users.
#[test]
fn scenario_b_two_macs_on_different_volumes() {
    let one = machine("/Users/alice", false, STUFF, "/Volumes/Data/Development/stuff");
    let two = machine("/Users/username", false, STUFF, "/Volumes/Users/username/Development/stuff");

    let a = one.tokenize_plain("/Volumes/Data/Development/stuff/src/lib.rs");
    let b = two.tokenize_plain("/Volumes/Users/username/Development/stuff/src/lib.rs");
    assert_eq!(a, format!("{STUFF_TOKEN}/src/lib.rs"));
    assert_eq!(a, b);
    assert_eq!(
        two.expand_plain(&a),
        "/Volumes/Users/username/Development/stuff/src/lib.rs"
    );
    // The look-alike home prefix must not be mistaken for `${HOME}`.
    assert!(!b.contains("${HOME}"));
}

/// Scenario C — two Windows machines where ONE copy happens to sit under the
/// user's home and the other doesn't. Without an identity these key
/// differently (`${HOME}\...` vs a literal `D:\...`); the git origin outranks
/// `${HOME}` and pulls both onto the same key.
#[test]
fn scenario_c_two_windows_one_home_relative_one_not() {
    let home_two = "C:\\Users\\username";
    let root_two = "C:\\Users\\username\\Development\\misc\\stuff";

    // Without a project map: machine two goes home-relative, machine one
    // cannot, so the keys diverge.
    let bare_one = Tokenizer::with_case_sensitivity("C:\\Users\\alice", true);
    let bare_two = Tokenizer::with_case_sensitivity(home_two, true);
    // Note the tail keeps NATIVE separators here — the `${HOME}` branch does
    // not normalize, unlike `${GIT}`/`${PROJ}` (see the dedicated test below).
    assert_eq!(
        bare_two.tokenize_plain(&format!("{root_two}\\README.md")),
        "${HOME}\\Development\\misc\\stuff\\README.md"
    );
    assert_ne!(
        bare_one.tokenize_plain("D:\\misc\\stuff\\README.md"),
        bare_two.tokenize_plain(&format!("{root_two}\\README.md")),
    );

    // With the git origin known on both, the identity wins over `${HOME}`.
    let one = machine("C:\\Users\\alice", true, STUFF, "D:\\misc\\stuff");
    let two = machine(home_two, true, STUFF, root_two);
    let a = one.tokenize_plain("D:\\misc\\stuff\\README.md");
    let b = two.tokenize_plain(&format!("{root_two}\\README.md"));
    assert_eq!(a, format!("{STUFF_TOKEN}/README.md"));
    assert_eq!(a, b);
    assert_eq!(one.expand_plain(&a), "D:\\misc\\stuff/README.md");
    assert_eq!(two.expand_plain(&a), format!("{root_two}/README.md"));
}

/// `${HOME}` keeps the ORIGIN machine's separators in its tail, while the
/// `${GIT}`/`${PROJ}` branches normalize theirs to `/`. That asymmetry is
/// deliberate — content rewriting needs the native shape back on expansion
/// (`dbsync::normalize_path_shape`, `registry::normalize_separators`) — but it
/// means a home-relative path is NOT cross-OS canonical by itself. Every call
/// site that turns one into a STORE KEY normalizes first (`codex.rs`,
/// `vscode.rs`). Pinned here so a future call site that forgets can't quietly
/// split one project into a Windows half and a macOS half.
#[test]
fn home_relative_keys_rely_on_call_site_separator_normalization() {
    let win = Tokenizer::with_case_sensitivity("C:\\Users\\you", true);
    let mac = Tokenizer::with_case_sensitivity("/Users/you", false);
    let w = win.tokenize_plain("C:\\Users\\you\\Documents\\notes\\a.md");
    let m = mac.tokenize_plain("/Users/you/Documents/notes/a.md");
    assert_eq!(w, "${HOME}\\Documents\\notes\\a.md");
    assert_eq!(m, "${HOME}/Documents/notes/a.md");
    assert_ne!(w, m, "raw tails differ by separator — keys must not use these directly");
    // What every key-producing call site actually does, and why it matters.
    assert_eq!(w.replace('\\', "/"), m);

    // The ENCODED form has no such problem: both separators become `-`, so
    // Claude's projects/ dir names unify across OSes with no help.
    assert_eq!(
        win.tokenize_encoded("C--Users-you-Documents-notes"),
        mac.tokenize_encoded("-Users-you-Documents-notes")
    );
}

/// Drive-letter case varies in real recorded cwds; scenario C must survive a
/// lowercase drive on the wire.
#[test]
fn scenario_c_lowercase_drive_still_matches() {
    let one = machine("C:\\Users\\alice", true, STUFF, "D:\\misc\\stuff");
    assert_eq!(
        one.tokenize_plain("d:\\misc\\stuff\\README.md"),
        format!("{STUFF_TOKEN}/README.md")
    );
}

/// The same three scenarios in Claude's ENCODED `projects/` directory names,
/// where every non-alphanumeric byte becomes `-` and a Windows path loses its
/// drive colon (`D:\misc\stuff` -> `D--misc-stuff`).
#[test]
fn scenarios_hold_for_encoded_project_dir_names() {
    let mac = machine("/Users/alice", false, STUFF, "/Volumes/Data/stuff");
    let win = machine("C:\\Users\\bob", true, STUFF, "D:\\misc\\stuff");

    let from_mac = mac.tokenize_encoded("-Volumes-Data-stuff-src");
    let from_win = win.tokenize_encoded("D--misc-stuff-src");
    assert_eq!(from_mac, format!("{STUFF_TOKEN}-src"));
    assert_eq!(from_mac, from_win);
    assert_eq!(mac.expand_encoded(&from_mac), "-Volumes-Data-stuff-src");
    assert_eq!(win.expand_encoded(&from_mac), "D--misc-stuff-src");
}

/// A machine that does not know the repo must PARK, never materialize the
/// token literally on disk — and must resolve the moment the root is learned.
#[test]
fn unknown_repo_parks_then_resolves_once_learned() {
    let key = format!("{STUFF_TOKEN}/notes.md");
    let cold = Tokenizer::with_case_sensitivity("/Users/alice", false);
    // Unexpandable: expand is a no-op and the token is still there.
    assert_eq!(cold.expand_plain(&key), key);
    assert!(vibesync_engine::gitmap::has_unresolved_token(&cold.expand_plain(&key)));

    let warm = machine("/Users/alice", false, STUFF, "/Volumes/Data/stuff");
    let resolved = warm.expand_plain(&key);
    assert_eq!(resolved, "/Volumes/Data/stuff/notes.md");
    assert!(!vibesync_engine::gitmap::has_unresolved_token(&resolved));
}

/// End to end on the real filesystem: a clone at a location no other machine
/// has published, never opened in a tool here, is found through a declared
/// code folder and its sessions key identically to the origin machine's.
#[test]
fn declared_code_folder_resolves_a_scenario_b_clone() {
    let tmp = tempfile::tempdir().unwrap();
    // Stand-in for /Volumes/Data/Development — deliberately NOT under home.
    let drive = tmp.path().join("Volumes").join("Data").join("Development");
    let proj = drive.join("stuff");
    std::fs::create_dir_all(&proj).unwrap();
    make_repo(&proj, "git@github.com:o/stuff.git");

    // Cold machine: home elsewhere, nothing learned, so the key won't resolve.
    let home = tmp.path().join("Users").join("alice");
    std::fs::create_dir_all(&home).unwrap();
    let home_s = home.to_string_lossy().into_owned();
    let cold = Tokenizer::with_case_sensitivity(&home_s, cfg!(windows));
    let key = format!("{STUFF_TOKEN}/notes.md");
    assert_eq!(cold.expand_plain(&key), key, "cold machine must park");

    // Declaring the code folder is the only thing that reaches this clone.
    let mut map = GitMap::default();
    for c in vibesync_engine::gitmap::find_clone_roots(&drive, 4) {
        if let Some((root, _)) = vibesync_engine::gitmap::discover(&c) {
            if root == c {
                map.learn(&c);
            }
        }
    }
    assert_eq!(map.roots.len(), 1, "the clone must be discovered");

    let warm = Tokenizer::with_case_sensitivity(&home_s, cfg!(windows)).with_gitmap(&map);
    assert_eq!(
        Path::new(&warm.expand_plain(&key)),
        proj.join("notes.md").as_path()
    );
    // And the round trip is canonical: this machine's path keys back to the
    // same token the origin machine produced.
    assert_eq!(
        warm.tokenize_plain(&proj.join("notes.md").to_string_lossy()),
        key
    );
}
