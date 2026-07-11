//! Push/pull orchestration.
//!
//! Rules (v1):
//! - Additive only: nothing is ever deleted from the store.
//! - Never clobber newer local content; the loser of a conflict is kept as a
//!   `.codesync-bak` sibling, so no bytes are ever lost.
//! - Files marked `deleted_locally` are never re-applied (retention rule).

use std::path::Path;

use anyhow::{Context, Result};
use filetime::FileTime;

use crate::adapters::Adapter;
use crate::scanner::{hash_bytes, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};
use crate::tokenizer::Tokenizer;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub pushed: usize,
    pub pulled: usize,
    pub skipped_newer_local: usize,
    pub skipped_deleted: usize,
    pub unchanged: usize,
}

/// Upload every scanned entry whose content the store doesn't have yet.
pub fn push(
    entries: &[FileEntry],
    state: &mut SyncState,
    store: &dyn SyncStore,
    source: &str,
) -> Result<Report> {
    let mut report = Report::default();
    for entry in entries {
        let known = state.files.get(&entry.logical);
        if let Some(st) = known {
            if st.hash == entry.hash && !st.deleted_locally {
                report.unchanged += 1;
                continue;
            }
        }
        let data = std::fs::read(&entry.abs)
            .with_context(|| format!("read {}", entry.abs.display()))?;
        // The file may have changed between scan and read; hash what we send.
        let hash = hash_bytes(&data);
        store.put(
            &entry.logical,
            &data,
            &RemoteMeta {
                hash: hash.clone(),
                mtime_ms: entry.mtime_ms,
                size: data.len() as u64,
                source: source.to_string(),
            },
        )?;
        state.files.insert(
            entry.logical.clone(),
            FileState {
                hash,
                mtime_ms: entry.mtime_ms,
                size: data.len() as u64,
                deleted_locally: false,
            },
        );
        report.pushed += 1;
    }
    Ok(report)
}

/// Download store entries this machine doesn't have (or has older versions of).
/// `include_optional` gates opt-in roots (e.g. plugins) on the pull side too.
pub fn pull(
    adapter: &Adapter,
    home: &Path,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    include_optional: bool,
) -> Result<Report> {
    let mut report = Report::default();
    for (logical, meta) in store.list()? {
        let Some(abs) = adapter.resolve(&logical, home, tok, include_optional) else {
            continue; // not this adapter's namespace (or opted out)
        };
        if let Some(st) = state.files.get(&logical) {
            if st.deleted_locally {
                report.skipped_deleted += 1;
                continue;
            }
            if st.hash == meta.hash {
                report.unchanged += 1;
                continue;
            }
        }
        if abs.exists() {
            let local_hash = crate::scanner::hash_file(&abs)?;
            if local_hash == meta.hash {
                // Already in sync; just record it.
                state.files.insert(
                    logical.clone(),
                    FileState {
                        hash: meta.hash.clone(),
                        mtime_ms: meta.mtime_ms,
                        size: meta.size,
                        deleted_locally: false,
                    },
                );
                report.unchanged += 1;
                continue;
            }
            let local_mtime = crate::scanner::mtime_ms(&abs)?;
            if local_mtime > meta.mtime_ms {
                report.skipped_newer_local += 1;
                continue;
            }
            // Local loses: keep its content as a backup sibling before replacing.
            let bak = abs.with_extension(match abs.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{ext}.codesync-bak"),
                None => "codesync-bak".to_string(),
            });
            std::fs::copy(&abs, &bak)?;
        }
        let Some((data, _)) = store.get(&logical)? else {
            continue;
        };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("codesync-tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &abs)?;
        filetime::set_file_mtime(
            &abs,
            FileTime::from_unix_time(meta.mtime_ms / 1000, ((meta.mtime_ms % 1000) * 1_000_000) as u32),
        )?;
        state.files.insert(
            logical.clone(),
            FileState {
                hash: meta.hash.clone(),
                mtime_ms: meta.mtime_ms,
                size: meta.size,
                deleted_locally: false,
            },
        );
        report.pulled += 1;
    }
    Ok(report)
}
