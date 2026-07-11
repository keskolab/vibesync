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
use crate::store::SyncStore;

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

fn home_norm(home: &Path) -> String {
    home.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_string()
}

/// Tokenize a normalized folder path against this machine's home.
/// Case-insensitive prefix match (Windows drive-letter case varies).
fn tokenize_folder(norm: &str, home: &str) -> String {
    if norm.len() >= home.len() && norm[..home.len()].eq_ignore_ascii_case(home) {
        let rest = &norm[home.len()..];
        if rest.is_empty() || rest.starts_with('/') {
            return format!("${{HOME}}{rest}");
        }
    }
    norm.to_string()
}

#[allow(dead_code)] // used by the upcoming state.vscdb index merge
fn expand_folder(tokenized: &str, home: &str) -> String {
    match tokenized.strip_prefix("${HOME}") {
        Some(rest) => format!("{home}{rest}"),
        None => tokenized.to_string(),
    }
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
pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    scan_roots(&storage_roots(), home)
}

pub fn scan_roots(roots: &[PathBuf], home: &Path) -> Result<Vec<FileEntry>> {
    let home = home_norm(home);
    let mut out = Vec::new();
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(root) else { continue };
        for ws in dirs.flatten() {
            if !ws.path().is_dir() {
                continue;
            }
            let Some(folder) = workspace_folder(&ws.path()) else { continue };
            let ws_key = sanitize(&tokenize_folder(&folder, &home));
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
fn local_workspace_map(roots: &[PathBuf], home: &str) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for root in roots {
        let Ok(dirs) = std::fs::read_dir(root) else { continue };
        for ws in dirs.flatten() {
            if let Some(folder) = workspace_folder(&ws.path()) {
                map.insert(sanitize(&tokenize_folder(&folder, home)), ws.path());
            }
        }
    }
    map
}

#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub applied: usize,
    pub parked: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
}

/// Apply `vscode/ws/...` store entries into matching local workspaces;
/// park the rest. Same conflict rules as the generic pull.
pub fn apply(
    store: &dyn SyncStore,
    state: &mut SyncState,
    home: &Path,
) -> Result<ApplyReport> {
    apply_roots(&storage_roots(), store, state, home)
}

pub fn apply_roots(
    roots: &[PathBuf],
    store: &dyn SyncStore,
    state: &mut SyncState,
    home: &Path,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    let home = home_norm(home);
    let map = local_workspace_map(roots, &home);
    let prefix = format!("{LOGICAL_PREFIX}/");

    for (logical, meta) in store.list()? {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        let comps: Vec<&str> = rest.split('/').collect();
        let Some(marker) = comps.iter().position(|c| SESSION_DIRS.contains(c)) else {
            continue;
        };
        if marker == 0 || marker + 1 >= comps.len() {
            continue;
        }
        let ws_key = comps[..marker].join("/");
        if let Some(st) = state.files.get(&logical) {
            if st.deleted_locally || st.hash == meta.hash {
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
        if abs.exists() {
            if hash_file(&abs)? == meta.hash {
                report.unchanged += 1;
                state.files.insert(
                    logical.clone(),
                    FileState { hash: meta.hash, mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
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
        let Some((data, _)) = store.get(&logical)? else { continue };
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
        report.applied += 1;
    }
    Ok(report)
}

/// Cheap counts for the UI: (chat files, bytes).
pub fn light_counts() -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    for root in storage_roots() {
        let Ok(dirs) = std::fs::read_dir(&root) else { continue };
        for ws in dirs.flatten() {
            let dir = ws.path().join("chatSessions");
            let Ok(files) = std::fs::read_dir(&dir) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if matches!(p.extension().and_then(|e| e.to_str()), Some("json") | Some("jsonl")) {
                    n += 1;
                    bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    (n, bytes)
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
            &format!("file://{}/dev/app", a_home.to_string_lossy()),
        );
        std::fs::create_dir_all(ws_a.join("chatSessions")).unwrap();
        std::fs::write(ws_a.join("chatSessions/s1.jsonl"), "{\"chat\":1}\n").unwrap();

        let entries = scan_roots(&[a_root.clone()], &a_home).unwrap();
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
            &format!("file://{}/dev/app", b_home.to_string_lossy()),
        );
        make_ws(&b_root, "cccc3333", "file:///somewhere/else");

        let mut state_b = SyncState::default();
        let report = apply_roots(&[b_root.clone()], &store, &mut state_b, &b_home).unwrap();
        assert_eq!(report.applied, 1);
        let landed = ws_b.join("chatSessions/s1.jsonl");
        assert!(landed.exists());
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "{\"chat\":1}\n");

        // Machine C: workspace absent -> parked, nothing written.
        let c_root = tmp.path().join("c_ws");
        std::fs::create_dir_all(&c_root).unwrap();
        let mut state_c = SyncState::default();
        let report = apply_roots(&[c_root], &store, &mut state_c, &tmp.path().join("c_home")).unwrap();
        assert_eq!(report.parked, 1);
        assert_eq!(report.applied, 0);
    }

    #[test]
    fn windows_uri_normalization() {
        assert_eq!(
            uri_to_norm("file:///c%3A/Users/you/dev").unwrap(),
            "c:/Users/you/dev"
        );
        assert_eq!(uri_to_norm("file:///Users/anna/dev").unwrap(), "/Users/anna/dev");
        let t = tokenize_folder("c:/Users/you/dev", "C:/Users/you");
        assert_eq!(t, "${HOME}/dev");
        assert_eq!(expand_folder(&t, "/Users/anna"), "/Users/anna/dev");
    }
}
