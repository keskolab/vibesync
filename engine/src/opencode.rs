//! OpenCode adapter (file layer).
//!
//! Storage (validated on macOS, 2026-07): OpenCode keeps per-record JSON
//! under `~/.local/share/opencode/storage/{session,message,part,...}/` AND a
//! relational SQLite db (`opencode.db`). Which is authoritative in the
//! current build is unconfirmed, so this adapter syncs ONLY the storage/
//! files (opaque, additive — records are id-keyed and never collide) and
//! never writes the db. That guarantees a safe cross-machine archive.
//!
//! KNOWN LIMIT: if the current OpenCode reads sessions from opencode.db
//! rather than storage/, synced records won't appear in its UI until a db
//! merge is added — pending a live injection test on a machine with OpenCode.
//! Interior `directory` paths inside records are not rewritten (same policy
//! as Claude transcripts).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scanner::{hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::SyncStore;

pub const PREFIX: &str = "opencode/storage";
/// Volatile/derived subdirs we never sync.
const SKIP: &[&str] = &["session_diff", "session_share", "migration"];

fn storage_root(home: &Path) -> Option<PathBuf> {
    for cand in [
        home.join(".local/share/opencode/storage"),
        dirs::data_dir().map(|d| d.join("opencode/storage")).unwrap_or_default(),
    ] {
        if cand.is_dir() {
            return Some(cand);
        }
    }
    None
}

pub fn detect(home: &Path) -> bool {
    storage_root(home).is_some()
}

pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let Some(root) = storage_root(home) else { return Ok(vec![]) };
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|e| {
        !(e.file_type().is_dir()
            && e.file_name().to_str().map(|n| SKIP.contains(&n)).unwrap_or(false))
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(FileEntry {
            logical: format!("{PREFIX}/{rel}"),
            abs: path.to_path_buf(),
            size: entry.metadata()?.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub pulled: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
}

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    // Apply even if the dir doesn't exist yet (first pull creates it).
    let root = home.join(".local/share/opencode/storage");
    let prefix = format!("{PREFIX}/");
    for (logical, meta) in store.list()? {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        if let Some(st) = state.files.get(&logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let mut abs = root.clone();
        for c in rest.split('/') {
            abs.push(c);
        }
        if abs.exists() {
            if hash_file(&abs)? == meta.hash {
                report.unchanged += 1;
                continue;
            }
            if mtime_ms(&abs)? > meta.mtime_ms {
                report.skipped_newer_local += 1;
                continue;
            }
        }
        let Some((data, _)) = store.get(&logical)? else { continue };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("vibesync-tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &abs)?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        report.pulled += 1;
    }
    Ok(report)
}

/// (session records, bytes, projects, newest mtime).
pub fn light_counts(home: &Path) -> (usize, u64, usize, Option<i64>) {
    let Some(root) = storage_root(home) else { return (0, 0, 0, None) };
    let mut sessions = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    for entry in walkdir::WalkDir::new(root.join("session")).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            sessions += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(m) = mtime_ms(p) {
                last = Some(last.map_or(m, |l: i64| l.max(m)));
            }
        }
    }
    let projects = walkdir::WalkDir::new(root.join("project"))
        .max_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .count();
    (sessions, bytes, projects, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn storage_files_sync_additively() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let s = a.join(".local/share/opencode/storage/session");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("ses_1.json"), "{\"id\":\"ses_1\"}").unwrap();
        // A volatile dir that must be skipped.
        let sd = a.join(".local/share/opencode/storage/session_diff");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("d.json"), "x").unwrap();

        let entries = scan(&a).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].logical.ends_with("session/ses_1.json"));
        let mut state_a = SyncState::default();
        crate::sync::push(&entries, &mut state_a, &store, "a").unwrap();

        let b = tmp.path().join("b");
        std::fs::create_dir_all(b.join(".local/share/opencode/storage")).unwrap();
        let mut state_b = SyncState::default();
        let report = apply(&b, &mut state_b, &store).unwrap();
        assert_eq!(report.pulled, 1);
        assert!(b.join(".local/share/opencode/storage/session/ses_1.json").exists());
    }
}
