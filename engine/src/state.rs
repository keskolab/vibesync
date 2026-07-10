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
