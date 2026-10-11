//! Project matching regression suite: how a local path becomes a store key,
//! and how a store key finds its way back onto this machine.
//!
//! Keys are shared with machines running OLDER builds, so the first section
//! pins the keys every already-correct case produces today — any change
//! there re-keys real users' sessions in the store. The sections after it
//! are one regression per matching bug fixed, each reproduced first.

use std::path::{Path, PathBuf};

use vibesync_engine::adapters::CLAUDE_CODE;
use vibesync_engine::gitmap::{self, GitMap};
use vibesync_engine::store::{FolderStore, RemoteMeta, SyncStore};
use vibesync_engine::tokenizer::{encode_cwd, Tokenizer};
use vibesync_engine::SyncState;

// ---------------------------------------------------------------- helpers

fn make_repo(dir: &Path, origin: &str) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/config"), format!("[remote \"origin\"]\n\turl = {origin}\n")).unwrap();
}

/// What `git worktree add <wt>` leaves behind: a `.git` FILE in the
/// worktree pointing at `<main>/.git/worktrees/<name>`, whose `commondir`
/// leads back to the main clone's `.git`.
fn make_worktree(main: &Path, wt: &Path, name: &str) {
    let gd = main.join(".git/worktrees").join(name);
    std::fs::create_dir_all(&gd).unwrap();
    std::fs::write(gd.join("commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn tok_for(home: &Path) -> Tokenizer {
    Tokenizer::with_case_sensitivity(&s(home), cfg!(windows))
}

/// Same map normalization `GitMap::learn` applies to a root.
fn root_str(p: &Path) -> String {
    s(p).trim_end_matches(['/', '\\']).to_string()
}

fn make_ws(root: &Path, hash: &str, folder: &Path) -> PathBuf {
    let ws = root.join(hash);
    std::fs::create_dir_all(ws.join("chatSessions")).unwrap();
    std::fs::write(
        ws.join("workspace.json"),
        format!("{{\"folder\": \"file://{}\"}}", s(folder).replace('\\', "/")),
    )
    .unwrap();
    ws
}

fn set_mtime(p: &Path, secs: i64) {
    filetime::set_file_mtime(p, filetime::FileTime::from_unix_time(secs, 0)).unwrap();
}

fn put(store: &dyn SyncStore, logical: &str, body: &[u8], mtime_ms: i64) {
    store
        .put(
            logical,
            body,
            &RemoteMeta {
                hash: vibesync_engine::scanner::hash_bytes(body),
                mtime_ms,
                size: body.len() as u64,
                source: "other".into(),
            },
        )
        .unwrap();
}

const INDEX_KEY: &str = "chat.ChatSessionStore.index";

fn make_vscdb(ws: &Path, index: Option<&str>) {
    let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    // VS Code's own schema.
    conn.execute("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)", [])
        .unwrap();
    if let Some(v) = index {
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", [INDEX_KEY, v]).unwrap();
    }
}

fn read_index_raw(ws: &Path) -> Option<String> {
    let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    conn.query_row("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?1", [INDEX_KEY], |r| r.get(0))
        .ok()
}

// ------------------------------------------------- pinned existing keys

/// Identities are stored on every machine (git_roots.json, the atlas, every
/// `${GIT}` key). Normal GitHub/GitLab/Bitbucket remotes must normalize
/// byte-for-byte as they always have.
#[test]
fn pin_identities_of_common_remotes() {
    for (url, id) in [
        ("git@github.com:JohnKesko/vibesync.git", "github.com/johnkesko/vibesync"),
        ("https://github.com/JohnKesko/vibesync.git", "github.com/johnkesko/vibesync"),
        ("https://github.com/JohnKesko/vibesync", "github.com/johnkesko/vibesync"),
        ("https://user@github.com/JohnKesko/vibesync/", "github.com/johnkesko/vibesync"),
        ("https://user:token@github.com/o/r.git", "github.com/o/r"),
        ("ssh://git@github.com:22/JohnKesko/vibesync.git", "github.com/johnkesko/vibesync"),
        ("ssh://git@github.com/o/r", "github.com/o/r"),
        ("git@gitlab.com:group/sub/project.git", "gitlab.com/group/sub/project"),
        ("https://gitlab.com/group/sub/project.git", "gitlab.com/group/sub/project"),
        ("https://gitlab.example.com:8443/team/app.git", "gitlab.example.com/team/app"),
        ("git@bitbucket.org:team/repo.git", "bitbucket.org/team/repo"),
        ("  git@github.com:o/r.git  ", "github.com/o/r"),
    ] {
        assert_eq!(gitmap::normalize_origin(url).as_deref(), Some(id), "{url}");
    }
    assert_eq!(gitmap::git_token("github.com/johnkesko/vibesync"), "${GIT:github.com:johnkesko:vibesync}");
    assert_eq!(
        gitmap::parse_git_token("${GIT:github.com:johnkesko:vibesync}/app"),
        Some(("github.com/johnkesko/vibesync".to_string(), "/app"))
    );
}

/// A git repo matched by its remote, on the real filesystem: learned from a
/// cwd, keyed `${GIT:...}` plain and encoded, expanded back to the clone.
#[test]
fn pin_git_repo_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dev").join("app");
    make_repo(&repo, "git@github.com:o/app.git");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let mut map = GitMap::default();
    assert!(map.learn(&repo.join("src")));
    assert_eq!(map.roots["github.com/o/app"], root_str(&repo));
    let tok = tok_for(&home).with_gitmap(&map);

    assert_eq!(tok.tokenize_plain(&s(&repo)), "${GIT:github.com:o:app}");
    assert_eq!(tok.tokenize_plain(&s(&repo.join("src"))), "${GIT:github.com:o:app}/src");
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&repo))), "${GIT:github.com:o:app}");
    // An existing subdirectory claims the encoded tail for the repo.
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&repo.join("src")))), "${GIT:github.com:o:app}-src");
    assert_eq!(Path::new(&tok.expand_plain("${GIT:github.com:o:app}/src")), repo.join("src"));
    assert_eq!(tok.expand_encoded("${GIT:github.com:o:app}-src"), encode_cwd(&s(&repo.join("src"))));
}

/// The same with string-only roots (another OS's paths, nothing on disk):
/// the shapes stored by today's fleet.
#[test]
fn pin_git_repo_keys_cross_os() {
    let mut mac_map = GitMap::default();
    mac_map.roots.insert("github.com/o/r".into(), "/Users/you/dev/r".into());
    let mac = Tokenizer::with_case_sensitivity("/Users/you", false).with_gitmap(&mac_map);
    let mut win_map = GitMap::default();
    win_map.roots.insert("github.com/o/r".into(), "D:\\code\\r".into());
    let win = Tokenizer::with_case_sensitivity("C:\\Users\\you", true).with_gitmap(&win_map);

    assert_eq!(mac.tokenize_plain("/Users/you/dev/r/app"), "${GIT:github.com:o:r}/app");
    assert_eq!(win.tokenize_plain("D:\\code\\r\\app"), "${GIT:github.com:o:r}/app");
    assert_eq!(win.tokenize_plain("d:\\code\\r\\app"), "${GIT:github.com:o:r}/app");
    assert_eq!(mac.tokenize_encoded("-Users-you-dev-r-app"), "${GIT:github.com:o:r}-app");
    assert_eq!(win.tokenize_encoded("D--code-r-app"), "${GIT:github.com:o:r}-app");
    assert_eq!(win.expand_encoded("${GIT:github.com:o:r}-app"), "D--code-r-app");
    assert_eq!(mac.expand_plain("${GIT:github.com:o:r}/app"), "/Users/you/dev/r/app");
}

/// A plain folder under home, on both OSes.
#[test]
fn pin_plain_folder_under_home() {
    let mac = Tokenizer::with_case_sensitivity("/Users/you", false);
    assert_eq!(mac.tokenize_plain("/Users/you/Documents/notes"), "${HOME}/Documents/notes");
    assert_eq!(mac.tokenize_encoded("-Users-you-Documents-notes"), "${EHOME}-Documents-notes");
    assert_eq!(mac.expand_encoded("${EHOME}-Documents-notes"), "-Users-you-Documents-notes");

    let win = Tokenizer::with_case_sensitivity("C:\\Users\\you", true);
    assert_eq!(win.tokenize_plain("C:\\Users\\you\\Documents\\notes"), "${HOME}\\Documents\\notes");
    assert_eq!(win.tokenize_encoded("C--Users-you-Documents-notes"), "${EHOME}-Documents-notes");
    assert_eq!(win.tokenize_encoded("c--Users-you-Documents-notes"), "${EHOME}-Documents-notes");
    assert_eq!(win.expand_encoded("${EHOME}-Documents-notes"), "C--Users-you-Documents-notes");

    // And on the real filesystem, where the folder exists.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let notes = home.join("Documents").join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    let tok = tok_for(&home);
    assert_eq!(tok.tokenize_plain(&s(&notes)).replace('\\', "/"), "${HOME}/Documents/notes");
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&notes))), "${EHOME}-Documents-notes");
}

/// A user project name mapping, plain and encoded, both OSes.
#[test]
fn pin_project_name_mapping() {
    let mut m = std::collections::BTreeMap::new();
    m.insert("stuff".to_string(), "/Volumes/Data/stuff".to_string());
    let mac = Tokenizer::with_case_sensitivity("/Users/you", false).with_manual_projects(&m);
    let mut w = std::collections::BTreeMap::new();
    w.insert("stuff".to_string(), "D:\\misc\\stuff".to_string());
    let win = Tokenizer::with_case_sensitivity("C:\\Users\\you", true).with_manual_projects(&w);

    assert_eq!(mac.tokenize_plain("/Volumes/Data/stuff/notes.md"), "${PROJ:stuff}/notes.md");
    assert_eq!(win.tokenize_plain("D:\\misc\\stuff\\notes.md"), "${PROJ:stuff}/notes.md");
    assert_eq!(mac.tokenize_encoded("-Volumes-Data-stuff-src"), "${PROJ:stuff}-src");
    assert_eq!(win.tokenize_encoded("D--misc-stuff-src"), "${PROJ:stuff}-src");
    assert_eq!(win.expand_encoded("${PROJ:stuff}-src"), "D--misc-stuff-src");
}

/// VS Code store keys: home-relative and git-identity workspaces.
#[test]
fn pin_vscode_workspace_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("ws");
    let plain = make_ws(&root, "aaaa", &home.join("dev").join("notes"));
    std::fs::write(plain.join("chatSessions/s1.jsonl"), "{}\n").unwrap();
    let repo = tmp.path().join("elsewhere").join("app");
    let ws_git = make_ws(&root, "bbbb", &repo);
    std::fs::write(ws_git.join("chatSessions/s2.jsonl"), "{}\n").unwrap();
    let mut map = GitMap::default();
    map.roots.insert("github.com/o/app".into(), s(&repo));
    let tok = tok_for(&home).with_gitmap(&map);

    let keys: Vec<String> =
        vibesync_engine::vscode::scan_roots(&[root], &tok).unwrap().into_iter().map(|e| e.logical).collect();
    assert_eq!(
        keys,
        vec![
            "vscode/ws/${GIT%3Agithub.com%3Ao%3Aapp}/chatSessions/s2.jsonl".to_string(),
            "vscode/ws/${HOME}/dev/notes/chatSessions/s1.jsonl".to_string(),
        ]
    );
}

// ------------------------------------- 1. VS Code duplicate workspace keys

/// Two workspace-storage folders for ONE folder (re-clone, Code + Insiders).
/// Returns (ws_root, older ws, newer ws, home).
fn duplicate_workspaces(tmp: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    duplicate_workspaces_named(tmp, "zzzz-old", "aaaa-new")
}

fn duplicate_workspaces_named(tmp: &Path, old_name: &str, new_name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let home = tmp.join("home");
    let folder = home.join("proj");
    let root = tmp.join("ws");
    let old = make_ws(&root, old_name, &folder);
    let new = make_ws(&root, new_name, &folder);
    std::fs::write(old.join("chatSessions/c1.jsonl"), "{\"v\":1}\n").unwrap();
    std::fs::write(new.join("chatSessions/c1.jsonl"), "{\"v\":2,\"more\":true}\n").unwrap();
    std::fs::write(old.join("chatSessions/only-old.jsonl"), "{\"o\":1}\n").unwrap();
    make_vscdb(&old, None);
    make_vscdb(&new, None);
    for f in [old.join("chatSessions/c1.jsonl"), old.join("chatSessions/only-old.jsonl")] {
        set_mtime(&f, 1_000_000);
    }
    set_mtime(&old.join("state.vscdb"), 1_000_000);
    set_mtime(&old.join("workspace.json"), 1_000_000);
    set_mtime(&new.join("chatSessions/c1.jsonl"), 2_000_000);
    set_mtime(&new.join("state.vscdb"), 2_000_000);
    set_mtime(&new.join("workspace.json"), 2_000_000);
    (root, old, new, home)
}

#[test]
fn vscode_duplicate_workspaces_scan_each_key_once_newest_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _old, new, home) = duplicate_workspaces(tmp.path());
    let tok = tok_for(&home);
    let entries = vibesync_engine::vscode::scan_roots(&[root], &tok).unwrap();
    let keys: Vec<&str> = entries.iter().map(|e| e.logical.as_str()).collect();
    assert_eq!(
        keys,
        vec!["vscode/ws/${HOME}/proj/chatSessions/c1.jsonl", "vscode/ws/${HOME}/proj/chatSessions/only-old.jsonl"],
        "one entry per logical key"
    );
    assert_eq!(entries[0].abs, new.join("chatSessions/c1.jsonl"), "newest copy wins");
}

#[test]
fn vscode_duplicate_workspaces_push_converges() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, _, _, home) = duplicate_workspaces(tmp.path());
    let tok = tok_for(&home);
    let store = FolderStore::new(tmp.path().join("store"));
    let mut st = SyncState::default();
    let first = vibesync_engine::sync::push(
        &vibesync_engine::vscode::scan_roots(std::slice::from_ref(&root), &tok).unwrap(),
        &mut st,
        &store,
        "me",
    )
    .unwrap();
    assert_eq!(first.pushed, 2);
    for _ in 0..3 {
        let r = vibesync_engine::sync::push(
            &vibesync_engine::vscode::scan_roots(std::slice::from_ref(&root), &tok).unwrap(),
            &mut st,
            &store,
            "me",
        )
        .unwrap();
        assert_eq!(r.pushed, 0, "the two copies must not alternate in the store");
    }
}

#[test]
fn vscode_apply_writes_into_the_most_recently_used_folder() {
    // Both name orders, so directory listing order can't make it pass.
    for (old_name, new_name) in [("zzzz-old", "aaaa-new"), ("aaaa-old", "zzzz-new")] {
        let tmp = tempfile::tempdir().unwrap();
        let (root, old, new, home) = duplicate_workspaces_named(tmp.path(), old_name, new_name);
        let store = FolderStore::new(tmp.path().join("store"));
        put(&store, "vscode/ws/${HOME}/proj/chatSessions/remote-1.jsonl", b"{\"r\":1}\n", 5_000);
        let mut st = SyncState::default();
        let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok_for(&home)).unwrap();
        assert_eq!(r.applied, 1);
        assert!(new.join("chatSessions/remote-1.jsonl").exists(), "{new_name}");
        assert!(!old.join("chatSessions/remote-1.jsonl").exists(), "{old_name}");
        // The panel index is reconciled in that folder only.
        assert!(read_index_raw(&new).is_some());
        assert!(read_index_raw(&old).is_none());
    }
}

/// Two conflicts on one chat file: the second backup must not overwrite the
/// first, and a backup identical to an existing one is reused.
#[test]
fn vscode_conflict_backups_are_numbered_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("ws");
    let ws = make_ws(&root, "aaaa", &home.join("proj"));
    let f = ws.join("chatSessions/c.jsonl");
    let store = FolderStore::new(tmp.path().join("store"));
    let tok = tok_for(&home);
    let mut st = SyncState::default();
    let key = "vscode/ws/${HOME}/proj/chatSessions/c.jsonl";
    for (i, (local, remote)) in
        [("LOCAL-ONE\n", "REMOTE-ONE\n"), ("LOCAL-TWO\n", "REMOTE-TWO\n"), ("LOCAL-TWO\n", "REMOTE-3\n")]
            .iter()
            .enumerate()
    {
        std::fs::write(&f, local).unwrap();
        set_mtime(&f, 1_000 + i as i64 * 10);
        put(&store, key, remote.as_bytes(), (1_005 + i as i64 * 10) * 1000);
        let r = vibesync_engine::vscode::apply_roots(std::slice::from_ref(&root), &store, &mut st, &tok).unwrap();
        assert_eq!(r.applied, 1, "round {i}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), *remote);
    }
    let dir = ws.join("chatSessions");
    assert_eq!(std::fs::read_to_string(dir.join("c.jsonl.vibesync-bak")).unwrap(), "LOCAL-ONE\n");
    assert_eq!(std::fs::read_to_string(dir.join("c.jsonl.2.vibesync-bak")).unwrap(), "LOCAL-TWO\n");
    assert!(!dir.join("c.jsonl.3.vibesync-bak").exists(), "identical backup reused");
}

// ------------------------------------- 2. worktrees and second clones

#[test]
fn worktree_is_never_an_alias_of_the_main_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dev").join("repo");
    let wt = repo.join(".claude").join("worktrees").join("feat");
    make_repo(&repo, "git@github.com:o/repo.git");
    make_worktree(&repo, &wt, "feat");

    let mut map = GitMap::default();
    assert!(map.learn(&repo));
    map.learn(&wt);
    assert!(map.aliases.get("github.com/o/repo").map(|a| a.is_empty()).unwrap_or(true), "{map:?}");
    assert_eq!(map.roots["github.com/o/repo"], root_str(&repo));

    // Nested in the main clone: keyed relative to it, so it expands back to
    // the worktree itself — not onto the main clone.
    let tok = tok_for(&home).with_gitmap(&map);
    let key = tok.tokenize_plain(&s(&wt));
    assert_eq!(key, "${GIT:github.com:o:repo}/.claude/worktrees/feat");
    assert_eq!(Path::new(&tok.expand_plain(&key)), wt.as_path());
    let ekey = tok.tokenize_encoded(&encode_cwd(&s(&wt)));
    assert_ne!(ekey, "${GIT:github.com:o:repo}");
    assert_eq!(tok.expand_encoded(&ekey), encode_cwd(&s(&wt)));
}

#[test]
fn worktree_learned_first_never_becomes_the_primary() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dev").join("repo");
    let nested = repo.join(".claude").join("worktrees").join("feat");
    let sibling = home.join("dev").join("repo-hotfix");
    make_repo(&repo, "git@github.com:o/repo.git");
    make_worktree(&repo, &nested, "feat");
    make_worktree(&repo, &sibling, "hotfix");

    let mut map = GitMap::default();
    map.learn(&sibling);
    assert!(map.roots.is_empty(), "a worktree is not a repo root: {map:?}");
    map.learn(&nested);
    // The nested worktree sits inside the main clone, so learning from it
    // finds the main clone — never the worktree.
    assert_eq!(map.roots.get("github.com/o/repo"), Some(&root_str(&repo)));
    assert!(map.aliases.is_empty(), "{map:?}");
    assert_eq!(gitmap::discover(&sibling), None);
}

#[test]
fn sibling_worktree_keys_like_a_plain_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dev").join("repo");
    let wt = home.join("dev").join("repo-feat");
    make_repo(&repo, "git@github.com:o/repo.git");
    make_worktree(&repo, &wt, "feat");
    let mut map = GitMap::default();
    map.learn(&repo);
    map.learn(&wt);
    let tok = tok_for(&home).with_gitmap(&map);
    assert_eq!(tok.tokenize_plain(&s(&wt)).replace('\\', "/"), "${HOME}/dev/repo-feat");
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&wt))), "${EHOME}-dev-repo-feat");
}

/// End to end through the Claude adapter: the worktree's transcripts keep
/// their own key, and that key resolves back to the worktree's project dir.
#[test]
fn worktree_sessions_stay_in_their_own_project_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("dev").join("repo");
    let wt = repo.join(".claude").join("worktrees").join("feat");
    make_repo(&repo, "git@github.com:o/repo.git");
    make_worktree(&repo, &wt, "feat");
    let mut map = GitMap::default();
    map.learn(&repo);
    map.learn(&wt);
    let tok = tok_for(&home).with_gitmap(&map);

    let pdir = home.join(".claude").join("projects");
    let (enc_main, enc_wt) = (encode_cwd(&s(&repo)), encode_cwd(&s(&wt)));
    std::fs::create_dir_all(pdir.join(&enc_main)).unwrap();
    std::fs::create_dir_all(pdir.join(&enc_wt)).unwrap();
    std::fs::write(pdir.join(&enc_main).join("main.jsonl"), "{}\n").unwrap();
    std::fs::write(pdir.join(&enc_wt).join("wt.jsonl"), "{}\n").unwrap();

    let entries = CLAUDE_CODE.scan(&home, &tok, false).unwrap();
    let wt_entry = entries.iter().find(|e| e.logical.ends_with("/wt.jsonl")).unwrap();
    let main_entry = entries.iter().find(|e| e.logical.ends_with("/main.jsonl")).unwrap();
    let dir_of = |l: &str| l.rsplit_once('/').unwrap().0.to_string();
    assert_ne!(dir_of(&wt_entry.logical), dir_of(&main_entry.logical));
    assert_eq!(
        CLAUDE_CODE.resolve(&wt_entry.logical, &home, &tok, false).unwrap(),
        pdir.join(&enc_wt).join("wt.jsonl")
    );
}

#[test]
fn second_full_clone_shares_the_repo_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let first = home.join("dev").join("first");
    let second = home.join("dev").join("second");
    make_repo(&first, "git@github.com:o/two.git");
    make_repo(&second, "git@github.com:o/two.git");
    let mut map = GitMap::default();
    assert!(map.learn(&first));
    assert!(map.learn(&second));
    assert_eq!(map.roots["github.com/o/two"], root_str(&first), "existing primary kept");

    // Both clones are the one project; expansion targets the primary.
    let tok = tok_for(&home).with_gitmap(&map);
    assert_eq!(tok.tokenize_plain(&s(&first)), "${GIT:github.com:o:two}");
    let k2 = tok.tokenize_plain(&s(&second.join("src")));
    assert_eq!(k2, "${GIT:github.com:o:two}/src");
    assert_eq!(Path::new(&tok.expand_plain(&k2)), first.join("src"));
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&second))), "${GIT:github.com:o:two}");
}

/// git_roots.json written by an older build may hold a worktree as the
/// primary (or as an alias). Loading promotes the real clone and drops the
/// worktree; a living second clone and a moved-away root stay aliases.
#[test]
fn legacy_map_with_living_aliases_is_repaired_on_load() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let main = home.join("dev").join("repo");
    let wt = home.join("dev").join("repo-feat");
    let second = home.join("dev").join("repo-copy");
    let gone = home.join("dev").join("old-spot");
    make_repo(&main, "git@github.com:o/repo.git");
    make_worktree(&main, &wt, "feat");
    make_repo(&second, "git@github.com:o/repo.git");

    let mut legacy = GitMap::default();
    let id = "github.com/o/repo".to_string();
    // A worktree learned first became the primary; the main clone, a
    // second clone and a genuinely moved-away root are aliases.
    legacy.roots.insert(id.clone(), s(&wt));
    legacy.aliases.insert(id.clone(), vec![s(&main), s(&second), s(&gone)]);
    let path = tmp.path().join("git_roots.json");
    legacy.save(&path).unwrap();

    let (map, trusted) = GitMap::load(&path);
    assert!(trusted);
    assert_eq!(map.roots[&id], s(&main), "the main clone is promoted over the worktree");
    assert_eq!(map.aliases[&id], vec![s(&second), s(&gone)], "the worktree is gone; the second clone and the moved-away root stay");
}

/// Another machine keeps the repo at the path where THIS machine has a
/// worktree: the fleet alias must not claim it. A second full clone of the
/// same repo there is claimed — it is the same project.
#[test]
fn fleet_alias_never_claims_a_local_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let main = home.join("dev").join("repo");
    let wt = home.join("dev").join("repo-feat");
    let second = home.join("dev").join("repo-copy");
    make_repo(&main, "git@github.com:o/repo.git");
    make_worktree(&main, &wt, "feat");
    make_repo(&second, "git@github.com:o/repo.git");
    let mut map = GitMap::default();
    map.learn(&main);
    let mut atlas = std::collections::BTreeMap::new();
    atlas.insert(
        "github.com/o/repo".to_string(),
        vec!["${HOME}/dev/repo-feat".to_string(), "${HOME}/dev/repo-copy".to_string()],
    );
    let tok = tok_for(&home).with_gitmap(&map).with_fleet_aliases(&atlas);
    assert_eq!(tok.tokenize_plain(&s(&wt)).replace('\\', "/"), "${HOME}/dev/repo-feat");
    assert_eq!(tok.tokenize_plain(&s(&second)), "${GIT:github.com:o:repo}");
}

#[test]
fn clone_hunt_skips_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let code = tmp.path().join("code");
    let main = code.join("repo");
    let wt = code.join("repo-feat");
    make_repo(&main, "git@github.com:o/repo.git");
    make_worktree(&main, &wt, "feat");
    assert_eq!(gitmap::find_clone_roots(&code, 3), vec![main]);
}

// ------------------------------------------------ 3. VS Code chat index

/// One workspace with a native chat, a state.vscdb holding `index`, and a
/// synced chat waiting in the store. Returns (ws_root, ws, store, tok).
fn index_setup(tmp: &Path, index: Option<&str>) -> (PathBuf, PathBuf, FolderStore, Tokenizer) {
    let home = tmp.join("home");
    let root = tmp.join("ws");
    let ws = make_ws(&root, "aaaa", &home.join("proj"));
    std::fs::write(ws.join("chatSessions/native-1.jsonl"), "{}\n").unwrap();
    make_vscdb(&ws, index);
    let store = FolderStore::new(tmp.join("store"));
    put(&store, "vscode/ws/${HOME}/proj/chatSessions/synced-1.jsonl", b"{\"s\":1}\n", 5_000);
    (root, ws, store, tok_for(&home))
}

#[test]
fn vscode_index_with_unexpected_shape_is_left_untouched() {
    for original in [
        // `entries` as an array (a future format): used to be wiped to {}.
        r#"{"version":1,"entries":[{"sessionId":"native-1","title":"My real title"}]}"#,
        // A version this code has never seen.
        r#"{"version":2,"entries":{"native-1":{"title":"My real title"}}}"#,
        // Not JSON at all.
        "{not json",
        // Valid JSON, not an object: `index["entries"] = ..` used to panic.
        r#"["native-1"]"#,
        r#""a string""#,
        r#"{"version":1}"#,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (root, ws, store, tok) = index_setup(tmp.path(), Some(original));
        let mut st = SyncState::default();
        let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
        assert_eq!(r.applied, 1, "the chat file still lands");
        assert!(ws.join("chatSessions/synced-1.jsonl").exists());
        assert_eq!(read_index_raw(&ws).as_deref(), Some(original), "index must be left as VS Code wrote it");
    }
}

#[test]
fn vscode_index_merge_preserves_every_existing_field() {
    let tmp = tempfile::tempdir().unwrap();
    let original = r#"{"version":1,"futureTopLevel":{"x":[1,2]},"entries":{"native-1":{"sessionId":"native-1","title":"My real title","lastMessageDate":42,"timing":{"created":1,"lastRequestEnded":2},"someNewField":true}}}"#;
    let (root, ws, store, tok) = index_setup(tmp.path(), Some(original));
    let mut st = SyncState::default();
    vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    let after: serde_json::Value = serde_json::from_str(&read_index_raw(&ws).unwrap()).unwrap();
    let before: serde_json::Value = serde_json::from_str(original).unwrap();
    assert_eq!(after["entries"]["native-1"], before["entries"]["native-1"]);
    assert_eq!(after["futureTopLevel"], before["futureTopLevel"]);
    assert_eq!(after["version"], 1);
    assert_eq!(after["entries"]["synced-1"]["sessionId"], "synced-1");
}

/// VS Code's ItemTable column is BLOB-typed; a value stored as a blob used
/// to fail the TEXT read, count as "no index" and get replaced wholesale.
#[test]
fn vscode_index_stored_as_blob_is_merged_not_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, ws, store, tok) = index_setup(tmp.path(), None);
    {
        let conn = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
        let v = br#"{"version":1,"entries":{"native-1":{"sessionId":"native-1","title":"My real title"}}}"#;
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", rusqlite::params![INDEX_KEY, v.to_vec()])
            .unwrap();
    }
    let mut st = SyncState::default();
    vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    let after: serde_json::Value = serde_json::from_str(&read_index_raw(&ws).unwrap()).unwrap();
    assert_eq!(after["entries"]["native-1"]["title"], "My real title");
    assert!(after["entries"]["synced-1"].is_object());
}

/// A db another process is writing (VS Code mid-flush) must not be read
/// as "no index": nothing is written, and the merge lands next sync.
#[test]
fn vscode_index_busy_db_is_skipped_then_converges() {
    let tmp = tempfile::tempdir().unwrap();
    let original = r#"{"version":1,"entries":{"native-1":{"sessionId":"native-1","title":"My real title"}}}"#;
    let (root, ws, store, tok) = index_setup(tmp.path(), Some(original));
    let lock = rusqlite::Connection::open(ws.join("state.vscdb")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut st = SyncState::default();
    let r = vibesync_engine::vscode::apply_roots(std::slice::from_ref(&root), &store, &mut st, &tok).unwrap();
    assert_eq!(r.applied, 1);
    drop(lock);
    assert_eq!(read_index_raw(&ws).as_deref(), Some(original));
    vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok).unwrap();
    let after: serde_json::Value = serde_json::from_str(&read_index_raw(&ws).unwrap()).unwrap();
    assert_eq!(after["entries"]["native-1"]["title"], "My real title");
    assert!(after["entries"]["synced-1"].is_object());
}

// ------------------------------------ 4. encoded names next to a known root

/// Repo `~/dev/app` plus look-alike siblings. Claude's encoding maps
/// `app-web`, `app.old` and `app copy` onto the same `-`-joined shape as
/// subfolders of `app`; the filesystem decides which reading is real.
fn sibling_layout(tmp: &Path) -> (PathBuf, PathBuf, Tokenizer) {
    let home = tmp.join("home");
    let app = home.join("dev").join("app");
    make_repo(&app, "git@github.com:o/app.git");
    let mut map = GitMap::default();
    map.learn(&app);
    let tok = tok_for(&home).with_gitmap(&map);
    (home, app, tok)
}

#[test]
fn encoded_sibling_of_a_repo_keys_like_its_plain_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _app, tok) = sibling_layout(tmp.path());
    for (name, ekey) in
        [("app-web", "${EHOME}-dev-app-web"), ("app.old", "${EHOME}-dev-app-old"), ("app copy", "${EHOME}-dev-app-copy")]
    {
        let sib = home.join("dev").join(name);
        std::fs::create_dir_all(sib.join("src")).unwrap();
        assert_eq!(tok.tokenize_plain(&s(&sib)).replace('\\', "/"), format!("${{HOME}}/dev/{name}"));
        assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&sib))), ekey, "{name}");
        assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&sib.join("src")))), format!("{ekey}-src"), "{name}");
        assert_eq!(tok.expand_encoded(ekey), encode_cwd(&s(&sib)));
    }
}

#[test]
fn encoded_subfolder_of_a_repo_is_still_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let (_home, app, tok) = sibling_layout(tmp.path());
    std::fs::create_dir_all(app.join("web")).unwrap();
    std::fs::create_dir_all(app.join("my.pkg").join("src")).unwrap();
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&app.join("web")))), "${GIT:github.com:o:app}-web");
    assert_eq!(
        tok.tokenize_encoded(&encode_cwd(&s(&app.join("my.pkg").join("src")))),
        "${GIT:github.com:o:app}-my-pkg-src"
    );
}

/// Both readings exist: the folder under the root wins.
#[test]
fn encoded_both_readings_exist_prefers_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, app, tok) = sibling_layout(tmp.path());
    std::fs::create_dir_all(app.join("web")).unwrap();
    std::fs::create_dir_all(home.join("dev").join("app-web")).unwrap();
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&app.join("web")))), "${GIT:github.com:o:app}-web");
}

/// Neither reading exists (a deleted folder): keep today's behavior.
#[test]
fn encoded_neither_reading_exists_keeps_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _app, tok) = sibling_layout(tmp.path());
    let ghost = home.join("dev").join("app-web");
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&ghost))), "${GIT:github.com:o:app}-web");
}

#[test]
fn encoded_sibling_of_a_project_mapping() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("Data");
    let stuff = data.join("stuff");
    let old = data.join("stuff-old");
    std::fs::create_dir_all(&stuff).unwrap();
    std::fs::create_dir_all(&old).unwrap();
    let mut m = std::collections::BTreeMap::new();
    m.insert("stuff".to_string(), s(&stuff));
    let tok = tok_for(&home).with_manual_projects(&m);
    assert_eq!(tok.tokenize_plain(&s(&old)), s(&old));
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&old))), encode_cwd(&s(&old)));
    // Once `stuff/old` exists too, the mapped root wins (a fresh tokenizer:
    // verdicts are memoized for the one sync a tokenizer lives for).
    std::fs::create_dir_all(stuff.join("old")).unwrap();
    let tok = tok_for(&home).with_manual_projects(&m);
    assert_eq!(tok.tokenize_encoded(&encode_cwd(&s(&stuff.join("old")))), "${PROJ:stuff}-old");
}

// ------------------------------------------- 5. a home folder that is a repo

#[test]
fn dotfiles_home_is_never_a_repo_root() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    make_repo(&home, "git@github.com:me/dotfiles.git");
    let notes = home.join("Documents").join("notes");
    std::fs::create_dir_all(&notes).unwrap();

    assert_eq!(gitmap::discover_with_home(&notes, Some(&home)), None);
    assert_eq!(gitmap::discover_with_home(&home, Some(&home)), None);
    let mut map = GitMap::default();
    assert!(!map.learn_with_home(&notes, Some(&home)));
    assert!(!map.learn_with_home(&home, Some(&home)));
    assert!(map.roots.is_empty());

    // A real project nested under the dotfiles home is still found.
    let proj = home.join("dev").join("proj");
    make_repo(&proj, "git@github.com:o/proj.git");
    assert_eq!(
        gitmap::discover_with_home(&proj.join("src"), Some(&home)),
        Some((proj.clone(), "github.com/o/proj".to_string()))
    );
}

/// A map persisted before the fix already holds the dotfiles root. The
/// tokenizer must not key every home folder under it.
#[test]
fn legacy_home_root_is_ignored() {
    let mut map = GitMap::default();
    map.roots.insert("github.com/me/dotfiles".into(), "/Users/me".into());
    map.roots.insert("github.com/o/r".into(), "/Users/me/dev/r".into());
    let tok = Tokenizer::with_case_sensitivity("/Users/me", false).with_gitmap(&map);
    assert_eq!(tok.tokenize_plain("/Users/me/Documents/notes"), "${HOME}/Documents/notes");
    assert_eq!(tok.tokenize_encoded("-Users-me-Documents-notes"), "${EHOME}-Documents-notes");
    assert_eq!(tok.tokenize_plain("/Users/me/dev/r/x"), "${GIT:github.com:o:r}/x");
    // Same through the fleet atlas.
    let mut atlas = std::collections::BTreeMap::new();
    atlas.insert("github.com/me/dotfiles".to_string(), vec!["${HOME}".to_string()]);
    let tok = Tokenizer::with_case_sensitivity("/Users/me", false).with_fleet_aliases(&atlas);
    assert_eq!(tok.tokenize_plain("/Users/me/Documents/notes"), "${HOME}/Documents/notes");

    let win_map = {
        let mut m = GitMap::default();
        m.roots.insert("github.com/me/dotfiles".into(), "c:\\users\\me".into());
        m
    };
    let win = Tokenizer::with_case_sensitivity("C:\\Users\\me", true).with_gitmap(&win_map);
    assert_eq!(win.tokenize_encoded("C--Users-me-Documents"), "${EHOME}-Documents");
}

#[test]
fn legacy_home_root_is_dropped_on_load() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    make_repo(&home, "git@github.com:me/dotfiles.git");
    let mut legacy = GitMap::default();
    legacy.roots.insert("github.com/me/dotfiles".into(), s(&home));
    assert!(legacy.repair_with_home(Some(&home)));
    assert!(legacy.roots.is_empty());
}

// --------------------------------------------- 6. remote normalization gaps

#[test]
fn azure_devops_remote_forms_agree() {
    let id = "dev.azure.com/org/proj/_git/repo";
    for url in [
        "https://dev.azure.com/org/proj/_git/repo",
        "https://org@dev.azure.com/org/proj/_git/repo",
        "git@ssh.dev.azure.com:v3/org/proj/repo",
        "ssh://git@ssh.dev.azure.com/v3/org/proj/repo",
        "https://org.visualstudio.com/proj/_git/repo",
        "https://org.visualstudio.com/DefaultCollection/proj/_git/repo",
        "org@vs-ssh.visualstudio.com:v3/org/proj/repo",
        "https://dev.azure.com/Org/Proj/_git/Repo",
    ] {
        assert_eq!(gitmap::normalize_origin(url).as_deref(), Some(id), "{url}");
    }
    // Spaces in project names are percent-encoded in both forms.
    assert_eq!(
        gitmap::normalize_origin("git@ssh.dev.azure.com:v3/org/My%20Proj/repo"),
        gitmap::normalize_origin("https://dev.azure.com/org/My%20Proj/_git/repo"),
    );
}

#[test]
fn quoted_config_url_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    for (line, id) in [
        ("url = \"git@github.com:o/quoted.git\"", "github.com/o/quoted"),
        ("url = git@github.com:o/commented.git ; set by setup", "github.com/o/commented"),
        ("url=\"https://github.com/o/q2\"  # note", "github.com/o/q2"),
    ] {
        let repo = tmp.path().join(id.replace('/', "_"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/config"), format!("[remote \"origin\"]\n\t{line}\n")).unwrap();
        assert_eq!(gitmap::discover(&repo).map(|(_, i)| i).as_deref(), Some(id), "{line}");
    }
}

#[test]
fn identity_with_a_colon_round_trips_through_its_token() {
    let id = gitmap::normalize_origin("https://example.com/team:x/repo.git").unwrap();
    assert!(id.contains(':'), "{id}");
    let token = gitmap::git_token(&id);
    let (back, rest) = gitmap::parse_git_token(&token).unwrap();
    assert_eq!(back, id);
    assert_eq!(rest, "");

    // And through the tokenizer: the key expands back onto the clone.
    let mut map = GitMap::default();
    map.roots.insert(id.clone(), "/Users/u/dev/repo".into());
    let tok = Tokenizer::with_case_sensitivity("/Users/u", false).with_gitmap(&map);
    let key = tok.tokenize_plain("/Users/u/dev/repo/src");
    assert_eq!(tok.expand_plain(&key), "/Users/u/dev/repo/src");
    let ekey = tok.tokenize_encoded("-Users-u-dev-repo-src");
    assert_eq!(tok.expand_encoded(&ekey), "-Users-u-dev-repo-src");
}

/// A chat marked deleted while it is on disk (marked by mistake, or
/// restored) still takes a newer copy from the store; a chat really gone
/// stays gone.
#[test]
fn vscode_a_marked_deleted_chat_on_disk_still_receives_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("ws");
    let ws = make_ws(&root, "aaaa", &home.join("proj"));
    let here = ws.join("chatSessions/c1.jsonl");
    std::fs::write(&here, "{\"old\":1}\n").unwrap();
    set_mtime(&here, 1);
    let store = FolderStore::new(tmp.path().join("store"));
    let k1 = "vscode/ws/${HOME}/proj/chatSessions/c1.jsonl";
    let k2 = "vscode/ws/${HOME}/proj/chatSessions/c2.jsonl";
    put(&store, k1, b"{\"new\":1}\n", 9_000_000);
    put(&store, k2, b"{\"gone\":1}\n", 9_000_000);
    let mut st = SyncState::default();
    for k in [k1, k2] {
        st.files.insert(
            k.to_string(),
            vibesync_engine::FileState { hash: "x".into(), mtime_ms: 1, size: 1, deleted_locally: true },
        );
    }
    let r = vibesync_engine::vscode::apply_roots(&[root], &store, &mut st, &tok_for(&home)).unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(std::fs::read_to_string(&here).unwrap(), "{\"new\":1}\n");
    assert!(!ws.join("chatSessions/c2.jsonl").exists(), "a really deleted chat stays deleted");
}
