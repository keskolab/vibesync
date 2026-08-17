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
    /// Entries waiting for a project/repo that isn't on this machine yet.
    pub parked: usize,
    /// Objects this pass could not take (undecryptable, unreachable, or
    /// unwritable). Skipped and retried next sync — never fatal.
    pub failed: usize,
}

/// True when an error means "this machine's passphrase cannot read that
/// object", i.e. it was written by a machine configured with a different
/// passphrase. Worth distinguishing from transient I/O: it never fixes
/// itself, and the fix is a human one.
pub fn is_undecryptable(msg: &str) -> bool {
    msg.contains("age decrypt") || msg.contains("No matching keys")
}

/// Fetch one object for an apply pass.
///
/// A failure here is ALWAYS per-object. One unreadable object must never
/// abort the pass: a single `?` used to collapse the whole batch, so
/// NOTHING applied and it looked exactly like "sync is dead" — live-hit
/// 2026-08-17, where four objects written under a different passphrase
/// blocked all ~10,700 others on every machine, every sync. State stays
/// untracked for a failure, so the object is retried on the next sync and
/// lands by itself once the cause is gone.
pub fn fetch_obj(store: &dyn SyncStore, logical: &str, failed: &mut usize) -> Option<Vec<u8>> {
    match store.get(logical) {
        Ok(Some((data, _))) => Some(data),
        // Vanished from the store between listing and fetch: not an error.
        Ok(None) => None,
        Err(e) => {
            *failed += 1;
            let msg = format!("{e:#}");
            if is_undecryptable(&msg) {
                // Collected for the end-of-sync diagnosis. Only the first
                // few are logged individually: a machine holding the wrong
                // passphrase cannot read ANY object, and ten thousand
                // identical warnings bury the one line that explains why.
                let n = note_unreadable(logical);
                if n <= 5 {
                    crate::dlog::warn(|| {
                        format!("cannot decrypt {logical} — written by a machine using a \
                                 different passphrase; skipping it")
                    });
                } else {
                    crate::dlog::debug(|| format!("cannot decrypt {logical}"));
                }
            } else {
                crate::dlog::warn(|| {
                    format!("fetch failed for {logical}: {msg} — skipping, retried next sync")
                });
            }
            None
        }
    }
}

/// Keys that failed to decrypt this sync, for the end-of-sync diagnosis.
static UNREADABLE: std::sync::LazyLock<std::sync::Mutex<Vec<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Record an undecryptable key; returns how many have been seen so far.
fn note_unreadable(logical: &str) -> usize {
    let mut v = UNREADABLE.lock().unwrap();
    // Cap the memory: the diagnosis only needs counts and sources.
    if v.len() < 20_000 {
        v.push(logical.to_string());
    }
    v.len()
}

/// Drain the keys collected since the last call.
pub fn take_unreadable() -> Vec<String> {
    std::mem::take(&mut *UNREADABLE.lock().unwrap())
}

/// Which machine's passphrase is wrong — the question that took a human two
/// days to answer by hand. The store's listing records who wrote each
/// object, so the engine can simply say it.
#[derive(Debug, Clone, PartialEq)]
pub enum PassphraseDiagnosis {
    /// Nothing unreadable.
    None,
    /// THIS machine cannot read most of the storage: its own passphrase is
    /// the odd one out, so nothing from the other machines arrives here.
    ThisMachine { unreadable: usize, total: usize, machines: Vec<(String, usize)> },
    /// A minority of objects are unreadable, all written elsewhere: that
    /// other machine is the misconfigured one.
    OtherMachine { machine: String, unreadable: usize },
}

/// Objects THIS machine uploaded that it can no longer read.
///
/// That combination has exactly one cause: they were encrypted with a
/// passphrase this machine no longer uses, so the copy in the store is
/// stale garbage while the local file is the good one. Forgetting the
/// upload (dropping the state entry) makes the next push re-send them
/// under the current passphrase — self-healing what otherwise needs a
/// human editing state.json by hand.
///
/// `is_me` decides whether a `RemoteMeta::source` names this machine; the
/// caller owns hostname canonicalization.
pub fn own_unreadable(
    unreadable: &[String],
    listing: &[(String, RemoteMeta)],
    is_me: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    if unreadable.is_empty() {
        return Vec::new();
    }
    let source_of: std::collections::HashMap<&str, &str> =
        listing.iter().map(|(k, m)| (k.as_str(), m.source.as_str())).collect();
    let mut out: Vec<String> = unreadable
        .iter()
        .filter(|k| source_of.get(k.as_str()).map(|s| is_me(s)).unwrap_or(false))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Classify a sync's decrypt failures. `listing` supplies the author of
/// each object (`RemoteMeta::source`).
pub fn diagnose_passphrase(
    unreadable: &[String],
    listing: &[(String, RemoteMeta)],
) -> PassphraseDiagnosis {
    if unreadable.is_empty() {
        return PassphraseDiagnosis::None;
    }
    let source_of: std::collections::HashMap<&str, &str> =
        listing.iter().map(|(k, m)| (k.as_str(), m.source.as_str())).collect();
    let mut by_machine: std::collections::HashMap<&str, usize> = Default::default();
    for k in unreadable {
        *by_machine.entry(source_of.get(k.as_str()).copied().unwrap_or("unknown")).or_default() +=
            1;
    }
    let mut machines: Vec<(String, usize)> =
        by_machine.into_iter().map(|(m, n)| (m.to_string(), n)).collect();
    machines.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // More than half the storage unreadable => the odd passphrase is ours.
    // (A single misconfigured peer can only poison what it has written.)
    let total = listing.len().max(unreadable.len());
    if unreadable.len() * 2 > total {
        PassphraseDiagnosis::ThisMachine { unreadable: unreadable.len(), total, machines }
    } else {
        PassphraseDiagnosis::OtherMachine {
            machine: machines[0].0.clone(),
            unreadable: unreadable.len(),
        }
    }
}

/// Upload every scanned entry whose content the store doesn't have yet.
pub fn push(
    entries: &[FileEntry],
    state: &mut SyncState,
    store: &dyn SyncStore,
    source: &str,
) -> Result<Report> {
    push_with_listing(entries, state, store, source, &[])
}

/// Push, skipping anything the store already holds byte-for-byte.
///
/// State alone is not enough to decide. A machine whose state was reset —
/// or which never recorded an apply (its downloads were failing) — has no
/// entry for files that are nonetheless already in the store, identical.
/// Uploading them again changes nothing but the object's ETag, which
/// invalidates every OTHER machine's listing cache and forces it to
/// re-download a metadata sidecar per object. Live-hit 2026-08-17: one
/// machine re-uploaded ~4,100 unchanged objects and every other machine's
/// sync went from 8 seconds to 67.
pub fn push_with_listing(
    entries: &[FileEntry],
    state: &mut SyncState,
    store: &dyn SyncStore,
    source: &str,
    listing: &[(String, RemoteMeta)],
) -> Result<Report> {
    use rayon::prelude::*;
    let mut report = Report::default();
    let remote_hash: std::collections::HashMap<&str, &str> =
        listing.iter().map(|(k, m)| (k.as_str(), m.hash.as_str())).collect();
    let mut adopt: Vec<(&FileEntry, &str)> = Vec::new();
    let todo: Vec<&FileEntry> = entries
        .iter()
        .filter(|e| {
            match state.files.get(&e.logical) {
                Some(st) if st.hash == e.hash && !st.deleted_locally => {
                    report.unchanged += 1;
                    return false;
                }
                // A deletion this machine recorded must stay recorded.
                Some(st) if st.deleted_locally => return true,
                _ => {}
            }
            // Content already in the store: adopt it into state instead of
            // re-uploading identical bytes.
            match remote_hash.get(e.logical.as_str()) {
                Some(h) if **h == e.hash => {
                    adopt.push((e, h));
                    report.unchanged += 1;
                    false
                }
                _ => true,
            }
        })
        .collect();
    if !adopt.is_empty() {
        crate::dlog::debug(|| {
            format!("push: {} file(s) already in the store — recorded, not re-uploaded", adopt.len())
        });
        for (e, hash) in adopt {
            state.files.insert(
                e.logical.clone(),
                FileState {
                    hash: (*hash).to_string(),
                    mtime_ms: e.mtime_ms,
                    size: e.size,
                    deleted_locally: false,
                },
            );
        }
    }
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
    pull_dir(adapter, home, ".claude", tok, state, store, include_optional, &|_| false, &listing, &|| {}, &|_| {})
}

/// Pull for one config dir (default `.claude` or a `.claude-*` profile).
#[allow(clippy::too_many_arguments)]
/// The one apply outcome every adapter reports — replaces six per-adapter
/// near-clones so the app maps results uniformly.
#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub applied: usize,
    pub unchanged: usize,
    pub skipped_newer_local: usize,
    /// Entries waiting for a project/repo that isn't on this machine yet.
    pub parked: usize,
    /// Objects this pass could not take (undecryptable or unreachable).
    /// Skipped and retried next sync — never fatal.
    pub failed: usize,
}

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
    on_pulled: &(dyn Fn(&str) + Sync),
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
        if crate::gitmap::has_unresolved_token(&abs.to_string_lossy()) {
            // Repo unknown on this machine: park in the store (state stays
            // untracked, so the entry is retried once the repo appears).
            crate::dlog::debug(|| format!("parked (project not on this machine): {logical}"));
            report.parked += 1;
            on_file();
            continue;
        }
        if let Some(st) = state.files.get(logical) {
            if st.deleted_locally {
                report.skipped_deleted += 1;
                on_file();
                continue;
            }
            // State is trusted only while the file is really there — a
            // synced-then-cleaned file must re-download, not skip forever.
            if st.hash == meta.hash && abs.exists() {
                report.unchanged += 1;
                on_file();
                continue;
            }
        }
        if abs.exists() {
            // A local file we cannot read (permissions, or one being
            // rewritten under us) is that file's problem, not the pass's.
            let local_hash = match crate::scanner::hash_file(&abs) {
                Ok(h) => h,
                Err(e) => {
                    crate::dlog::warn(|| {
                        format!("cannot read local {}: {e:#} — skipping", abs.display())
                    });
                    report.failed += 1;
                    on_file();
                    continue;
                }
            };
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
            let local_mtime = match crate::scanner::mtime_ms(&abs) {
                Ok(m) => m,
                Err(e) => {
                    crate::dlog::warn(|| {
                        format!("cannot stat local {}: {e:#} — skipping", abs.display())
                    });
                    report.failed += 1;
                    on_file();
                    continue;
                }
            };
            if local_mtime > meta.mtime_ms {
                report.skipped_newer_local += 1;
                on_file();
                continue;
            }
            // Local loses: keep its content as a backup sibling before replacing.
            crate::dlog::debug(|| format!("conflict: backing up {}", abs.display()));
            let bak = abs.with_extension(match abs.extension().and_then(|e| e.to_str()) {
                Some(ext) => format!("{ext}.vibesync-bak"),
                None => "vibesync-bak".to_string(),
            });
            // No backup, no overwrite — "nothing is ever lost" outranks
            // applying this one file.
            if let Err(e) = std::fs::copy(&abs, &bak) {
                crate::dlog::warn(|| {
                    format!("cannot back up {}: {e:#} — leaving local copy alone", abs.display())
                });
                report.failed += 1;
                on_file();
                continue;
            }
        }
        to_fetch.push((logical, meta, abs));
    }
    // Pass 2: parallel downloads — the slow half of a first sync. Failures
    // are per-object (see fetch_obj): the batch always completes.
    use rayon::prelude::*;
    let fetch_failures = std::sync::atomic::AtomicUsize::new(0);
    let fetched: Vec<(usize, Option<Vec<u8>>)> = to_fetch
        .par_iter()
        .enumerate()
        .map(|(i, (logical, _, _))| {
            let mut failed = 0usize;
            let data = fetch_obj(store, logical, &mut failed);
            if failed > 0 {
                fetch_failures.fetch_add(failed, std::sync::atomic::Ordering::Relaxed);
            }
            on_file();
            (i, data)
        })
        .collect();
    report.failed += fetch_failures.load(std::sync::atomic::Ordering::Relaxed);
    // Pass 3 (serial): write files and update state in order.
    for (i, data) in fetched {
        let (logical, meta, abs) = &to_fetch[i];
        let Some(data) = data else { continue };
        // One unwritable path (permissions, a full disk, a folder that
        // vanished) costs that file only — state stays untracked, so the
        // next sync tries again.
        if let Err(e) = write_pulled(abs, &data, meta.mtime_ms) {
            crate::dlog::warn(|| {
                format!("cannot write {}: {e:#} — skipping, retried next sync", abs.display())
            });
            report.failed += 1;
            continue;
        }
        state.files.insert(
            (*logical).clone(),
            FileState {
                hash: meta.hash.clone(),
                mtime_ms: meta.mtime_ms,
                size: meta.size,
                deleted_locally: false,
            },
        );
        on_pulled(logical);
        report.pulled += 1;
    }
    Ok(report)
}

/// Materialize one pulled object: temp file, atomic rename, store mtime.
/// The temp file is cleaned up on failure so a half-written pull can never
/// be mistaken for content.
fn write_pulled(abs: &Path, data: &[u8], mtime_ms: i64) -> Result<()> {
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::dlog::debug(|| format!("writing {}", abs.display()));
    let tmp = abs.with_extension("vibesync-tmp");
    let write = (|| -> Result<()> {
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, abs)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write;
    }
    filetime::set_file_mtime(
        abs,
        FileTime::from_unix_time(mtime_ms / 1000, ((mtime_ms % 1000) * 1_000_000) as u32),
    )?;
    Ok(())
}
