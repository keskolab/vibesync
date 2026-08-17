//! VS Code Copilot Chat adapter (file layer).
//!
//! Storage (validated on real machines, 2026-07):
//! - `<config>/<variant>/User/workspaceStorage/<md5>/chatSessions/*.jsonl`
//!   (current) and `*.json` (legacy) hold the conversations;
//!   `chatEditingSessions/<uuid>/**` are sidecars.
//! - `workspace.json` in each hash dir maps the dir to a folder URI
//!   (URL-encoded, e.g. `file:///c%3A/Users/x/dev/app`).
//! - The hash dir name derives from the folder path, so it differs per
//!   machine. We therefore key store objects by the TOKENIZED folder path,
//!   and on apply look up the matching local workspace by its folder.
//! - Sessions whose workspace doesn't exist locally are parked (kept in the
//!   store, not applied) — orphaned workspaces are the majority case in
//!   real data.
//!
//! KNOWN LIMIT (next milestone): VS Code lists chats from an index in each
//! workspace's `state.vscdb`, so applied files don't appear in the Chat
//! history panel until the index merge lands. The file layer still gives a
//! complete cross-machine archive and correct placement.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use filetime::FileTime;

use crate::scanner::{hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const LOGICAL_PREFIX: &str = "vscode/ws";
const VARIANTS: &[&str] = &["Code", "Code - Insiders"];
const SESSION_DIRS: &[&str] = &["chatSessions", "chatEditingSessions"];

/// `<config>/<variant>/User/workspaceStorage` for every installed variant.
pub fn storage_roots() -> Vec<PathBuf> {
    let Some(cfg) = dirs::config_dir() else { return vec![] };
    VARIANTS
        .iter()
        .map(|v| cfg.join(v).join("User").join("workspaceStorage"))
        .filter(|p| p.exists())
        .collect()
}

pub fn detect() -> bool {
    !storage_roots().is_empty()
}

/// `file:///c%3A/Users/x/dev` → `c:/Users/x/dev`;
/// `file:///Users/x/dev` → `/Users/x/dev`.
fn uri_to_norm(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8_lossy();
    let mut p = decoded.replace('\\', "/");
    // Windows URIs look like /c:/Users/... — drop the leading slash.
    let bytes = p.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
        p.remove(0);
    }
    Some(p)
}

/// Tokenize a workspace folder through the REAL tokenizer — git identity
/// first, then ${HOME} — so the same repo maps across machines regardless
/// of clone location or OS. File URIs decode with forward slashes; the
/// tokenizer matches native shapes, so normalize first; the resulting KEY
/// canonicalizes tail separators to '/' so both OSes produce identical
/// store keys.
fn tokenize_folder(norm: &str, tok: &crate::tokenizer::Tokenizer) -> String {
    let native = crate::dbsync::normalize_path_shape(norm);
    tok.tokenize_plain(&native).replace('\\', "/")
}

#[allow(dead_code)] // used by the upcoming state.vscdb index merge
fn expand_folder(tokenized: &str, tok: &crate::tokenizer::Tokenizer) -> String {
    crate::dbsync::normalize_path_shape(&tok.expand_plain(tokenized))
}

/// Workspace folders on this machine — fed to the project map so repos
/// used only through VS Code still gain their git identity.
pub fn local_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in storage_roots() {
        let Ok(dirs) = std::fs::read_dir(&root) else { continue };
        for ws in dirs.flatten() {
            if let Some(folder) = workspace_folder(&ws.path()) {
                let p = PathBuf::from(crate::dbsync::normalize_path_shape(&folder));
                if p.is_dir() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Store-safe path components (':' is illegal in Windows file names).
fn sanitize(s: &str) -> String {
    s.replace(':', "%3A")
}
fn unsanitize(s: &str) -> String {
    s.replace("%3A", ":")
}

fn workspace_folder(ws_dir: &Path) -> Option<String> {
    let meta = std::fs::read(ws_dir.join("workspace.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&meta).ok()?;
    uri_to_norm(v.get("folder")?.as_str()?)
}

/// Scan every variant's workspaces for chat files.
pub fn scan(tok: &crate::tokenizer::Tokenizer) -> Result<Vec<FileEntry>> {
    scan_roots(&storage_roots(), tok)
}

pub fn scan_roots(roots: &[PathBuf], tok: &crate::tokenizer::Tokenizer) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(root) else { continue };
        for ws in dirs.flatten() {
            if !ws.path().is_dir() {
                continue;
            }
            let Some(folder) = workspace_folder(&ws.path()) else { continue };
            let ws_key = sanitize(&tokenize_folder(&folder, tok));
            for sub in SESSION_DIRS {
                let dir = ws.path().join(sub);
                if !dir.exists() {
                    continue;
                }
                for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
                    let entry = entry?;
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let path = entry.path();
                    let ok = matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("json") | Some("jsonl")
                    );
                    if !ok {
                        continue;
                    }
                    let rel = path.strip_prefix(&dir)?;
                    let rel = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    out.push(FileEntry {
                        logical: format!("{LOGICAL_PREFIX}/{ws_key}/{sub}/{rel}"),
                        abs: path.to_path_buf(),
                        size: entry.metadata()?.len(),
                        mtime_ms: mtime_ms(path)?,
                        hash: hash_file(path)?,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

/// folder-path (tokenized, sanitized) → local workspace hash dir.
fn local_workspace_map(roots: &[PathBuf], tok: &crate::tokenizer::Tokenizer) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(root) else { continue };
        for ws in dirs.flatten() {
            if let Some(folder) = workspace_folder(&ws.path()) {
                map.insert(sanitize(&tokenize_folder(&folder, tok)), ws.path());
            }
        }
    }
    map
}

/// Apply `vscode/ws/...` store entries into matching local workspaces;
/// park the rest. Same conflict rules as the generic pull.
pub fn apply(
    store: &dyn SyncStore,
    state: &mut SyncState,
    tok: &crate::tokenizer::Tokenizer,
    merge_index: bool,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    apply_roots_opts(&storage_roots(), store, state, tok, merge_index, listing, on_file, on_pulled)
}

pub fn apply_roots(
    roots: &[PathBuf],
    store: &dyn SyncStore,
    state: &mut SyncState,
    tok: &crate::tokenizer::Tokenizer,
) -> Result<crate::sync::ApplyReport> {
    let listing = store.list()?;
    apply_roots_opts(roots, store, state, tok, true, &listing, &|| {}, &|_| {})
}

pub fn apply_roots_opts(
    roots: &[PathBuf],
    store: &dyn SyncStore,
    state: &mut SyncState,
    tok: &crate::tokenizer::Tokenizer,
    merge_index: bool,
    listing: &[(String, RemoteMeta)],
    on_file: &dyn Fn(),
    on_pulled: &dyn Fn(&str),
) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let map = local_workspace_map(roots, tok);
    let prefix = format!("{LOGICAL_PREFIX}/");

    for (logical, meta) in listing {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        let comps: Vec<&str> = rest.split('/').collect();
        let Some(marker) = comps.iter().position(|c| SESSION_DIRS.contains(c)) else {
            continue;
        };
        if marker == 0 || marker + 1 >= comps.len() {
            continue;
        }
        let ws_key = comps[..marker].join("/");
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally {
                report.unchanged += 1;
                continue;
            }
        }
        let Some(ws_dir) = map.get(&ws_key) else {
            report.parked += 1; // workspace not on this machine (yet)
            continue;
        };
        let mut abs = ws_dir.clone();
        for c in &comps[marker..] {
            abs.push(unsanitize(c));
        }
        if let Some(st) = state.files.get(logical) {
            // State is trusted only while the file is really there — a
            // synced-then-cleaned file must re-download, not skip forever.
            if st.deleted_locally || (st.hash == meta.hash && abs.exists()) {
                report.unchanged += 1;
                continue;
            }
        }
        if abs.exists() {
            if hash_file(&abs)? == meta.hash {
                report.unchanged += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
                );
                continue;
            }
            if mtime_ms(&abs)? > meta.mtime_ms {
                report.skipped_newer_local += 1;
                continue;
            }
            let bak = abs.with_extension(match abs.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{ext}.vibesync-bak"),
                None => "vibesync-bak".to_string(),
            });
            std::fs::copy(&abs, &bak)?;
        }
        let Some(data) = crate::sync::fetch_obj(store, logical, &mut report.failed) else { continue };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("vibesync-tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &abs).context("apply vscode chat")?;
        filetime::set_file_mtime(
            &abs,
            FileTime::from_unix_time(meta.mtime_ms / 1000, ((meta.mtime_ms % 1000) * 1_000_000) as u32),
        )?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }
    // Index reconcile — every sync, not just for newly-applied files.
    // VS Code holds state.vscdb open and flushes its in-memory index over
    // our external merges whenever it likes (live case on Windows: entries
    // merged mid-sync were gone minutes later, and nothing ever re-added
    // them because the files already counted as applied). Rather than fight
    // the write race, converge: any session file missing from its
    // workspace's panel index is re-added; a no-op when nothing was
    // clobbered.
    if merge_index {
        for ws_dir in map.values() {
            let Ok(rd) = std::fs::read_dir(ws_dir.join("chatSessions")) else { continue };
            let mut all: Vec<(String, i64)> = Vec::new();
            for e in rd.flatten() {
                let p = e.path();
                let ok = matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("json") | Some("jsonl")
                );
                if !ok {
                    continue;
                }
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
                all.push((stem.to_string(), mtime_ms(&p).unwrap_or(0)));
            }
            if all.is_empty() {
                continue;
            }
            match merge_chat_index(ws_dir, &all) {
                Ok(n) if n > 0 => {
                    crate::dlog::info(|| {
                        format!("vscode: {n} chats merged into the panel index")
                    });
                }
                Ok(_) => {
                    crate::dlog::debug(|| {
                        format!(
                            "vscode: panel index already complete ({} sessions) in {}",
                            all.len(),
                            ws_dir.display()
                        )
                    });
                }
                // db locked or absent: files are placed; index next sync
                Err(e) => {
                    crate::dlog::debug(|| {
                        format!("vscode: index reconcile skipped ({e}) in {}", ws_dir.display())
                    });
                }
            }
        }
    }
    Ok(report)
}

/// Best-effort title from the first chunk of a chat session file.
fn extract_title(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0u8; 64 * 1024];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    for key in ["\"customTitle\":\"", "\"title\":\""] {
        if let Some(i) = text.find(key) {
            let rest = &text[i + key.len()..];
            if let Some(j) = rest.find('"') {
                let t = &rest[..j];
                if !t.is_empty() && t.len() < 120 {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Register synced sessions in `chat.ChatSessionStore.index` inside the
/// workspace's state.vscdb (the key VS Code's history panel reads — found by
/// dumping a live db). Existing entries are never modified. Returns how many
/// new entries were added.
fn merge_chat_index(ws_dir: &Path, sessions: &[(String, i64)]) -> Result<usize> {
    const KEY: &str = "chat.ChatSessionStore.index";
    let db_path = ws_dir.join("state.vscdb");
    if !db_path.exists() {
        anyhow::bail!("no state.vscdb");
    }
    let conn = rusqlite::Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;
    let existing: Option<String> = conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [KEY], |r| r.get(0))
        .ok();
    let mut index: serde_json::Value = existing
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({ "version": 1, "entries": {} }));
    if !index.get("entries").map(|e| e.is_object()).unwrap_or(false) {
        index["entries"] = serde_json::json!({});
    }
    let entries = index["entries"].as_object_mut().unwrap();
    let mut added = 0usize;
    for (uuid, mtime) in sessions {
        if entries.contains_key(uuid) {
            continue;
        }
        let title = extract_title(&ws_dir.join("chatSessions").join(format!("{uuid}.jsonl")))
            .or_else(|| extract_title(&ws_dir.join("chatSessions").join(format!("{uuid}.json"))))
            .unwrap_or_else(|| "Synced chat".to_string());
        entries.insert(
            uuid.clone(),
            serde_json::json!({
                "sessionId": uuid,
                "title": title,
                "lastMessageDate": mtime,
                "timing": { "created": mtime },
                "initialLocation": "panel",
                "hasPendingEdits": false,
                "isEmpty": false,
                "isExternal": false,
                "lastResponseState": 1,
                "permissionLevel": "default"
            }),
        );
        added += 1;
    }
    if added > 0 {
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![KEY, serde_json::to_string(&index)?],
        )?;
    }
    Ok(added)
}

/// Cheap counts for the UI: (chat files, bytes, workspaces with chats,
/// newest chat mtime).
pub fn light_counts() -> (usize, u64, usize, Option<i64>) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut projects = 0usize;
    let mut last: Option<i64> = None;
    for root in storage_roots() {
        let Ok(dirs) = std::fs::read_dir(&root) else { continue };
        for ws in dirs.flatten() {
            let dir = ws.path().join("chatSessions");
            let Ok(files) = std::fs::read_dir(&dir) else { continue };
            let mut here = 0usize;
            for f in files.flatten() {
                let p = f.path();
                if matches!(p.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl")) {
                    here += 1;
                    bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Ok(m) = crate::scanner::mtime_ms(&p) {
                        last = Some(last.map_or(m, |l: i64| l.max(m)));
                    }
                }
            }
            if here > 0 {
                projects += 1;
            }
            n += here;
        }
    }
    (n, bytes, projects, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    fn make_ws(root: &Path, hash: &str, folder_uri: &str) -> PathBuf {
        let ws = root.join(hash);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("workspace.json"),
            format!("{{\"folder\": \"{folder_uri}\"}}"),
        )
        .unwrap();
        ws
    }

    #[test]
    fn cross_machine_workspace_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));

        // Machine A (Windows-style): workspace for c:\Users\anna\dev\app
        let a_home = tmp.path().join("a_home");
        let a_root = tmp.path().join("a_ws");
        let ws_a = make_ws(
            &a_root,
            "aaaa1111",
            // Forward slashes: a raw Windows tempdir path would put `\U`
            // escapes inside the JSON literal and break parsing. Real VS Code
            // URIs are always forward-slash.
            &format!("file://{}/dev/app", a_home.to_string_lossy().replace('\\', "/")),
        );
        std::fs::create_dir_all(ws_a.join("chatSessions")).unwrap();
        std::fs::write(ws_a.join("chatSessions/s1.jsonl"), "{\"chat\":1}\n").unwrap();

        let tok_a = crate::tokenizer::Tokenizer::with_case_sensitivity(&a_home.to_string_lossy(), false);
        let entries = scan_roots(&[a_root.clone()], &tok_a).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].logical.starts_with("vscode/ws/${HOME}/dev/app/chatSessions/"),
            "{}",
            entries[0].logical
        );
        let mut state_a = SyncState::default();
        crate::sync::push(&entries, &mut state_a, &store, "a").unwrap();

        // Machine B: same project folder exists (different hash), plus an
        // unrelated workspace.
        let b_home = tmp.path().join("b_home");
        let b_root = tmp.path().join("b_ws");
        let ws_b = make_ws(
            &b_root,
            "bbbb2222",
            &format!("file://{}/dev/app", b_home.to_string_lossy().replace('\\', "/")),
        );
        make_ws(&b_root, "cccc3333", "file:///somewhere/else");

        // B has a state.vscdb with an existing index entry that must survive.
        {
            let conn = rusqlite::Connection::open(ws_b.join("state.vscdb")).unwrap();
            conn.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
            conn.execute(
                "INSERT INTO ItemTable VALUES ('chat.ChatSessionStore.index',
                 '{\"version\":1,\"entries\":{\"native-1\":{\"sessionId\":\"native-1\",\"title\":\"Mine\"}}}')",
                [],
            ).unwrap();
        }
        let mut state_b = SyncState::default();
        let tok_b = crate::tokenizer::Tokenizer::with_case_sensitivity(&b_home.to_string_lossy(), false);
        let report = apply_roots(&[b_root.clone()], &store, &mut state_b, &tok_b).unwrap();
        assert_eq!(report.applied, 1);

        let landed = ws_b.join("chatSessions/s1.jsonl");
        assert!(landed.exists());
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "{\"chat\":1}\n");
        // Index now holds both the native entry and the synced one.
        let conn = rusqlite::Connection::open(ws_b.join("state.vscdb")).unwrap();
        let v: String = conn
            .query_row("SELECT value FROM ItemTable WHERE key='chat.ChatSessionStore.index'", [], |r| r.get(0))
            .unwrap();
        let idx: serde_json::Value = serde_json::from_str(&v).unwrap();
        assert!(idx["entries"]["native-1"]["title"] == "Mine");
        assert!(idx["entries"]["s1"]["sessionId"] == "s1");

        // Machine C: workspace absent -> parked, nothing written.
        let c_root = tmp.path().join("c_ws");
        std::fs::create_dir_all(&c_root).unwrap();
        let mut state_c = SyncState::default();
        let tok_c = crate::tokenizer::Tokenizer::with_case_sensitivity(
            &tmp.path().join("c_home").to_string_lossy(),
            false,
        );
        let report = apply_roots(&[c_root], &store, &mut state_c, &tok_c).unwrap();
        assert_eq!(report.parked, 1);
        assert_eq!(report.applied, 0);
    }

    #[test]
    fn git_identity_maps_workspaces_across_clone_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        const ID: &str = "github.com/o/app";

        // A keeps the repo at <a_home>/Desktop/app; B at <b_home>/dev/x/app.
        let a_home = tmp.path().join("a_home");
        let b_home = tmp.path().join("b_home");
        let mut map_a = crate::gitmap::GitMap::default();
        map_a.roots.insert(
            ID.into(),
            a_home.join("Desktop").join("app").to_string_lossy().into_owned(),
        );
        let mut map_b = crate::gitmap::GitMap::default();
        map_b.roots.insert(
            ID.into(),
            b_home.join("dev").join("x").join("app").to_string_lossy().into_owned(),
        );
        let tok_a = crate::tokenizer::Tokenizer::with_case_sensitivity(&a_home.to_string_lossy(), false)
            .with_gitmap(&map_a);
        let tok_b = crate::tokenizer::Tokenizer::with_case_sensitivity(&b_home.to_string_lossy(), false)
            .with_gitmap(&map_b);

        let a_root = tmp.path().join("a_ws2");
        let ws_a = make_ws(
            &a_root,
            "aaaa9999",
            &format!("file://{}/Desktop/app", a_home.to_string_lossy().replace('\\', "/")),
        );
        std::fs::create_dir_all(ws_a.join("chatSessions")).unwrap();
        std::fs::write(ws_a.join("chatSessions/g1.jsonl"), "{\"chat\":9}\n").unwrap();
        let entries = scan_roots(&[a_root], &tok_a).unwrap();
        assert!(
            entries[0].logical.starts_with("vscode/ws/${GIT%3Agithub.com%3Ao%3Aapp}/chatSessions/"),
            "{}",
            entries[0].logical
        );
        let mut st_a = SyncState::default();
        crate::sync::push(&entries, &mut st_a, &store, "a").unwrap();

        let b_root = tmp.path().join("b_ws2");
        let ws_b = make_ws(
            &b_root,
            "bbbb9999",
            &format!("file://{}/dev/x/app", b_home.to_string_lossy().replace('\\', "/")),
        );
        let mut st_b = SyncState::default();
        let report = apply_roots(&[b_root], &store, &mut st_b, &tok_b).unwrap();
        assert_eq!(report.applied, 1);
        assert!(ws_b.join("chatSessions/g1.jsonl").exists());
    }

    #[test]
    fn windows_uri_normalization() {
        assert_eq!(
            uri_to_norm("file:///c%3A/Users/you/dev").unwrap(),
            "c:/Users/you/dev"
        );
        assert_eq!(uri_to_norm("file:///Users/anna/dev").unwrap(), "/Users/anna/dev");
        // Cross-OS: a Windows URI-decoded folder tokenizes against a
        // Windows home and expands on a Mac — through the real tokenizer.
        let tok_w = crate::tokenizer::Tokenizer::with_case_sensitivity("C:\\Users\\you", true);
        let t = tokenize_folder("c:/Users/you/dev", &tok_w);
        assert_eq!(t, "${HOME}/dev");
        let tok_m = crate::tokenizer::Tokenizer::with_case_sensitivity("/Users/anna", false);
        assert_eq!(expand_folder(&t, &tok_m), "/Users/anna/dev");
    }
}
