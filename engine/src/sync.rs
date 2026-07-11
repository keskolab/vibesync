//! Push/pull orchestration.
//!
//! Rules (v1):
//! - Additive only: nothing is ever deleted from the store.
//! - Never clobber newer local content; the loser of a conflict is kept as a
//!   `.vibesync-bak` sibling, so no bytes are ever lost.
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
    use rayon::prelude::*;
    let mut report = Report::default();
    let todo: Vec<&FileEntry> = entries
        .iter()
        .filter(|e| {
            match state.files.get(&e.logical) {
                Some(st) if st.hash == e.hash && !st.deleted_locally => {
                    report.unchanged += 1;
                    false
                }
                _ => true,
            }
        })
        .collect();
    // Encrypt + upload in parallel; state is updated afterwards, serially.
    let results: Vec<Result<(String, FileState)>> = todo
        .par_iter()
        .map(|entry| {
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
            Ok((
                entry.logical.clone(),
                FileState {
                    hash,
                    mtime_ms: entry.mtime_ms,
                    size: data.len() as u64,
                    deleted_locally: false,
                },
            ))
        })
        .collect();
    for r in results {
        let (logical, st) = r?;
        state.files.insert(logical, st);
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
    let listing = store.list()?;
    pull_dir(adapter, home, ".claude", tok, state, store, include_optional, &|_| false, &listing, &|| {})
}

/// Pull for one config dir (default `.claude` or a `.claude-*` profile).
#[allow(clippy::too_many_arguments)]
pub fn pull_dir(
    adapter: &Adapter,
    home: &Path,
    dir: &str,
    tok: &Tokenizer,
    state: &mut SyncState,
    store: &dyn SyncStore,
    include_optional: bool,
    skip: &dyn Fn(&str) -> bool,
    listing: &[(String, RemoteMeta)],
    on_file: &(dyn Fn() + Sync),
) -> Result<Report> {
    let mut report = Report::default();
    // Pass 1 (serial, cheap): classify every entry in our namespace; collect
    // the downloads so pass 2 can fetch them in parallel.
    let mut to_fetch: Vec<(&String, &RemoteMeta, std::path::PathBuf)> = Vec::new();
    for (logical, meta) in listing {
        if skip(logical) {
            continue;
        }
        let Some(abs) = adapter.resolve_dir(logical, home, dir, tok, include_optional) else {
            continue; // not this adapter's namespace (or opted out)
        };
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally {
                report.skipped_deleted += 1;
                on_file();
                continue;
            }
            if st.hash == meta.hash {
                report.unchanged += 1;
                on_file();
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
                on_file();
                continue;
            }
            let local_mtime = crate::scanner::mtime_ms(&abs)?;
            if local_mtime > meta.mtime_ms {
                report.skipped_newer_local += 1;
                on_file();
                continue;
            }
            // Local loses: keep its content as a backup sibling before replacing.
            let bak = abs.with_extension(match abs.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{ext}.vibesync-bak"),
                None => "vibesync-bak".to_string(),
            });
            std::fs::copy(&abs, &bak)?;
        }
        to_fetch.push((logical, meta, abs));
    }
    // Pass 2: parallel downloads — the slow half of a first sync.
    use rayon::prelude::*;
    let fetched: Vec<(usize, Option<Vec<u8>>)> = to_fetch
        .par_iter()
        .enumerate()
        .map(|(i, (logical, _, _))| {
            let data = store.get(logical).map(|o| o.map(|(d, _)| d));
            on_file();
            data.map(|d| (i, d))
        })
        .collect::<Result<Vec<_>>>()?;
    // Pass 3 (serial): write files and update state in order.
    for (i, data) in fetched {
        let (logical, meta, abs) = &to_fetch[i];
        let Some(data) = data else { continue };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("vibesync-tmp");
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, abs)?;
        filetime::set_file_mtime(
            abs,
            FileTime::from_unix_time(meta.mtime_ms / 1000, ((meta.mtime_ms % 1000) * 1_000_000) as u32),
        )?;
        state.files.insert(
            (*logical).clone(),
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
