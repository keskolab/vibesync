//! GitHub Copilot CLI adapter.
//!
//! Storage (validated on macOS, 2026-07): `~/.copilot/` holds
//! - `session-state/<uuid>/` — one dir per session (state + metadata files).
//!   UUID-named, so files sync purely additively across machines.
//! - `logs/`, `restart/`, `ide/` — volatile, never synced.
//! - `config.json`, `settings.json` — local auth/trust settings, never synced.
//! - `vscode.session.*` — caches for the VS Code bridge, never synced.
//!
//! Sessions opened through the VS Code integration keep their chat content in
//! VS Code's own storage (covered by the vscode adapter); this adapter makes
//! standalone `copilot` CLI sessions follow the user. Interior absolute paths
//! inside session state are not rewritten (same policy as Claude transcripts).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scanner::{hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const PREFIX: &str = "copilot/session-state";

fn root(home: &Path) -> PathBuf {
    home.join(".copilot")
}

/// Installed = ~/.copilot holds something a real install writes (config,
/// logs, settings). A dir with only session-state/ is sync residue.
pub fn detect(home: &Path) -> bool {
    let dir = root(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        crate::dlog::debug(|| format!("detect copilot: NOT installed ({} missing)", dir.display()));
        return false;
    };
    let found = rd.flatten().any(|e| e.file_name() != "session-state");
    crate::dlog::debug(|| {
        format!(
            "detect copilot: {} ({})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            dir.display()
        )
    });
    found
}

pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let dir = root(home).join("session-state");
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(&dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(&dir)?
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

pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    let dir = root(home).join("session-state");
    let prefix = format!("{PREFIX}/");
    for (logical, meta) in listing {
        let Some(rest) = logical.strip_prefix(&prefix) else { continue };
        on_file();
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally || st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        let mut abs = dir.clone();
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
        let Some((data, _)) = store.get(logical)? else { continue };
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
        on_pulled(logical);
        report.pulled += 1;
    }
    Ok(report)
}

/// (sessions, bytes, newest activity ms).
pub fn light_counts(home: &Path) -> (usize, u64, Option<i64>) {
    let dir = root(home).join("session-state");
    let mut sessions = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            sessions += 1;
            for f in walkdir::WalkDir::new(e.path()).into_iter().flatten() {
                if f.file_type().is_file() {
                    bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Ok(m) = mtime_ms(f.path()) {
                        last = Some(last.map_or(m, |l: i64| l.max(m)));
                    }
                }
            }
        }
    }
    (sessions, bytes, last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn sync_residue_is_not_an_install() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".copilot/session-state/abc")).unwrap();
        assert!(!detect(home), "session-state alone is sync residue");
        std::fs::write(home.join(".copilot/config.json"), "{}").unwrap();
        assert!(detect(home), "a real install marker flips detection");
    }

    #[test]
    fn sessions_sync_additively_and_skip_volatile() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));
        let a = tmp.path().join("a");
        let s = a.join(".copilot/session-state/5ebe-uuid");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("state.json"), "{\"title\":\"hello\"}").unwrap();
        // Volatile/local files that must never sync.
        std::fs::create_dir_all(a.join(".copilot/logs")).unwrap();
        std::fs::write(a.join(".copilot/logs/x.log"), "x").unwrap();
        std::fs::write(a.join(".copilot/config.json"), "{}").unwrap();

        let entries = scan(&a).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].logical.ends_with("5ebe-uuid/state.json"));
        let mut state_a = SyncState::default();
        crate::sync::push(&entries, &mut state_a, &store, "a").unwrap();

        let b = tmp.path().join("b");
        std::fs::create_dir_all(b.join(".copilot")).unwrap();
        let mut state_b = SyncState::default();
        let listing = store.list().unwrap();
        let report = apply(&b, &mut state_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(report.pulled, 1);
        assert!(b.join(".copilot/session-state/5ebe-uuid/state.json").exists());
    }
}
