//! Per-machine sync state: what we've seen, synced, and deleted locally.
//!
//! The `deleted_locally` flag is the retention rule: a file the local machine
//! once had and later deleted (e.g. Claude Code's ~30-day cleanup) is never
//! pulled back down, while remaining in the remote archive.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileState {
    pub hash: String,
    pub mtime_ms: i64,
    pub size: u64,
    #[serde(default)]
    pub deleted_locally: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncState {
    /// Keyed by logical path.
    pub files: BTreeMap<String, FileState>,
}

impl SyncState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read(path).with_context(|| format!("read state {}", path.display()))?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Mark state entries under `logical_prefix` that are absent from the
    /// current scan as locally deleted. Returns how many were newly marked.
    ///
    /// CALLER CONTRACT: only call this when the scan's logical keys are
    /// trustworthy. A key here is derived through the project map, so an
    /// unreadable `git_roots.json` re-keys every path (`${GIT:id}/...` ->
    /// `${EHOME}-...`) and every old key then looks deleted. `deleted_locally`
    /// is one-way and permanently suppresses the pull, so a caller that can't
    /// vouch for its keying must skip this entirely rather than pass a scan it
    /// doesn't trust — see `GitMap::load`'s `trusted` flag.
    pub fn mark_deletions(&mut self, logical_prefix: &str, present: &[crate::FileEntry]) -> usize {
        let present: std::collections::BTreeSet<&str> =
            present.iter().map(|e| e.logical.as_str()).collect();
        let prefix = format!("{logical_prefix}/");
        let mut marked = 0;
        for (logical, st) in self.files.iter_mut() {
            if logical.starts_with(&prefix) && !st.deleted_locally && !present.contains(logical.as_str()) {
                st.deleted_locally = true;
                marked += 1;
            }
        }
        marked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(logical: &str) -> crate::FileEntry {
        crate::FileEntry {
            logical: logical.to_string(),
            abs: std::path::PathBuf::from("/tmp/x"),
            size: 1,
            mtime_ms: 1,
            hash: "h".into(),
        }
    }

    fn tracked_state(keys: &[&str]) -> SyncState {
        let mut s = SyncState::default();
        for k in keys {
            s.files.insert(
                (*k).to_string(),
                FileState { hash: "h".into(), mtime_ms: 1, size: 1, deleted_locally: false },
            );
        }
        s
    }

    #[test]
    fn marks_a_genuinely_missing_file() {
        let mut s = tracked_state(&["claude/projects/p/a.jsonl", "claude/projects/p/b.jsonl"]);
        // `a` still scanned, `b` really gone.
        let marked = s.mark_deletions("claude/projects", &[entry("claude/projects/p/a.jsonl")]);
        assert_eq!(marked, 1);
        assert!(!s.files["claude/projects/p/a.jsonl"].deleted_locally);
        assert!(s.files["claude/projects/p/b.jsonl"].deleted_locally);
    }

    #[test]
    fn a_wiped_prefix_is_marked_in_full() {
        // Retention cleanup can legitimately take the last transcript. This
        // is why the re-keying guard lives at the CALLER (gitmap_trusted)
        // and not here: suppressing the mark whenever a prefix comes back
        // empty would resurrect files the user's tool deliberately deleted.
        let mut s = tracked_state(&["claude/projects/p/a.jsonl"]);
        assert_eq!(s.mark_deletions("claude/projects", &[]), 1);
        assert!(s.files["claude/projects/p/a.jsonl"].deleted_locally);
    }

    #[test]
    fn marking_is_idempotent() {
        let mut s = tracked_state(&["claude/projects/p/a.jsonl"]);
        assert_eq!(s.mark_deletions("claude/projects", &[]), 1);
        assert_eq!(s.mark_deletions("claude/projects", &[]), 0);
    }

    #[test]
    fn other_prefixes_are_untouched() {
        let mut s = tracked_state(&["claude/projects/p/a.jsonl", "vscode/ws/w.json"]);
        s.mark_deletions("claude/projects", &[entry("claude/projects/p/a.jsonl")]);
        assert!(!s.files["vscode/ws/w.json"].deleted_locally);
    }
}
