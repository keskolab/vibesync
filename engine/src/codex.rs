//! Codex adapter.
//!
//! Storage (validated on macOS, 2026-07):
//! - Sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` —
//!   date-partitioned, filename carries a unique uuid, so files never
//!   collide and sync purely additively. No home-dependent path components,
//!   nothing to tokenize.
//! - `~/.codex/session_index.jsonl` drives the Desktop app's session list
//!   (one JSON object per line: `{id, thread_name, updated_at}`), and it is
//!   incomplete relative to the files on disk — so listing is INDEX-DRIVEN.
//!
//! To make every machine's Codex list the union of all sessions, each
//! machine publishes its own index under `codex/index/<machine>.jsonl` and,
//! on apply, merges every machine's index (union by id, newest wins) into the
//! local one. Session files sync as plain content under `codex/sessions/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::scanner::{hash_file, mtime_ms, FileEntry};
use crate::state::{FileState, SyncState};
use crate::store::{RemoteMeta, SyncStore};

pub const SESSIONS_PREFIX: &str = "codex/sessions";
const INDEX_PREFIX: &str = "codex/index";

fn root(home: &Path) -> PathBuf {
    home.join(".codex")
}

/// Installed = ~/.codex holds something a real install writes (auth, config,
/// logs). Pre-gating VibeSync versions created sessions/ + the index on
/// machines without Codex — such residue must not read as an install.
pub fn detect(home: &Path) -> bool {
    let dir = root(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        crate::dlog::debug(|| format!("detect codex: NOT installed ({} missing)", dir.display()));
        return false;
    };
    let found = rd.flatten().any(|e| {
        let n = e.file_name();
        n != "sessions" && n != "session_index.jsonl"
    });
    crate::dlog::debug(|| {
        format!(
            "detect codex: {} ({} {})",
            if found { "installed" } else { "NOT installed (only sync residue)" },
            dir.display(),
            if found { "has real install files" } else { "" }
        )
    });
    found
}

/// Scan session files (index is handled separately on push/apply).
pub fn scan(home: &Path) -> Result<Vec<FileEntry>> {
    let dir = root(home).join("sessions");
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
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let rel = path
            .strip_prefix(&dir)?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        out.push(FileEntry {
            logical: format!("{SESSIONS_PREFIX}/{rel}"),
            abs: path.to_path_buf(),
            size: entry.metadata()?.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}

fn index_path(home: &Path) -> PathBuf {
    root(home).join("session_index.jsonl")
}

/// Parse a JSONL index into id -> (updated_at, raw line).
fn parse_index(bytes: &[u8]) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let updated =
                    v.get("updated_at").and_then(|x| x.as_str()).unwrap_or("").to_string();
                map.insert(id.to_string(), (updated, line.to_string()));
            }
        }
    }
    map
}

/// Push session files (via the generic pusher, caller-supplied entries) is
/// handled by the caller; here we publish this machine's index object.
pub fn push_index(
    home: &Path,
    machine: &str,
    state: &mut SyncState,
    store: &dyn SyncStore,
) -> Result<()> {
    let path = index_path(home);
    if !path.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(&path)?;
    let hash = crate::scanner::hash_bytes(&bytes);
    let logical = format!("{INDEX_PREFIX}/{}.jsonl", sanitize(machine));
    if state.files.get(&logical).map(|s| s.hash == hash).unwrap_or(false) {
        return Ok(());
    }
    let mtime = mtime_ms(&path).unwrap_or(0);
    store.put(
        &logical,
        &bytes,
        &RemoteMeta { hash: hash.clone(), mtime_ms: mtime, size: bytes.len() as u64, source: machine.to_string() },
    )?;
    state.files.insert(
        logical,
        FileState { hash, mtime_ms: mtime, size: bytes.len() as u64, deleted_locally: false },
    );
    Ok(())
}

/// Apply session files this machine lacks, then union every machine's index
/// into the local session_index.jsonl (never dropping local entries).
pub fn apply(home: &Path, state: &mut SyncState, store: &dyn SyncStore, listing: &[(String, RemoteMeta)], on_file: &dyn Fn(), on_pulled: &dyn Fn(&str)) -> Result<crate::sync::ApplyReport> {
    let mut report = crate::sync::ApplyReport::default();
    let sessions_root = root(home).join("sessions");
    let index_prefix = format!("{INDEX_PREFIX}/");
    let session_prefix = format!("{SESSIONS_PREFIX}/");

    // Merge starts from the local index so we never lose local sessions.
    let mut merged = std::fs::read(index_path(home))
        .map(|b| parse_index(&b))
        .unwrap_or_default();

    for (logical, meta) in listing {
        // Index objects: union in.
        if let Some(_machine) = logical.strip_prefix(&index_prefix) {
            on_file();
            if let Some((bytes, _)) = store.get(logical)? {
                for (id, (updated, line)) in parse_index(&bytes) {
                    match merged.get(&id) {
                        Some((cur, _)) if *cur >= updated => {}
                        _ => {
                            if merged.insert(id.clone(), (updated, line)).is_none() {
                                crate::dlog::debug(|| {
                                    format!("codex: merged session {id} into local index")
                                });
                            }
                        }
                    }
                }
            }
            continue;
        }
        // Session files.
        let Some(rest) = logical.strip_prefix(&session_prefix) else { continue };
        on_file();
        let mut abs = sessions_root.clone();
        for c in rest.split('/') {
            abs.push(c);
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
        filetime::set_file_mtime(
            &abs,
            filetime::FileTime::from_unix_time(meta.mtime_ms / 1000, ((meta.mtime_ms % 1000) * 1_000_000) as u32),
        )?;
        state.files.insert(
            logical.clone(),
            FileState { hash: meta.hash.clone(), mtime_ms: meta.mtime_ms, size: meta.size, deleted_locally: false },
        );
        on_pulled(logical);
        report.applied += 1;
    }

    // Write the unioned index back (atomic).
    if !merged.is_empty() {
        let body = merged.values().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n") + "\n";
        let path = index_path(home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("jsonl.vibesync-tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(report)
}

/// Cheap counts: (session files, bytes, day-dirs, newest mtime).
pub fn light_counts(home: &Path) -> (usize, u64, usize, Option<i64>) {
    let dir = root(home).join("sessions");
    let mut n = 0;
    let mut bytes = 0u64;
    let mut last: Option<i64> = None;
    let mut days = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            n += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(m) = mtime_ms(p) {
                last = Some(last.map_or(m, |l: i64| l.max(m)));
            }
            if let Some(parent) = p.parent() {
                days.insert(parent.to_path_buf());
            }
        }
    }
    (n, bytes, days.len(), last)
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderStore;

    #[test]
    fn sync_residue_is_not_an_install() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".codex/sessions/2026/01/01")).unwrap();
        std::fs::write(home.join(".codex/session_index.jsonl"), "{}\n").unwrap();
        assert!(!detect(home), "sessions+index alone are sync residue");
        std::fs::write(home.join(".codex/config.toml"), "").unwrap();
        assert!(detect(home), "a real install marker flips detection");
    }

    #[test]
    fn sessions_sync_and_index_unions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path().join("store"));

        // Machine A: one session + index listing it.
        let a = tmp.path().join("a");
        let sess_a = a.join(".codex/sessions/2026/04/21");
        std::fs::create_dir_all(&sess_a).unwrap();
        std::fs::write(sess_a.join("rollout-aaa.jsonl"), "{\"m\":1}\n").unwrap();
        std::fs::write(
            a.join(".codex/session_index.jsonl"),
            "{\"id\":\"aaa\",\"thread_name\":\"A\",\"updated_at\":\"2026-04-21T09:00:00Z\"}\n",
        )
        .unwrap();

        let mut state_a = SyncState::default();
        crate::sync::push(&scan(&a).unwrap(), &mut state_a, &store, "a").unwrap();
        push_index(&a, "machine-a", &mut state_a, &store).unwrap();

        // Machine B: its own session + index; then apply A's.
        let b = tmp.path().join("b");
        let sess_b = b.join(".codex/sessions/2026/05/01");
        std::fs::create_dir_all(&sess_b).unwrap();
        std::fs::write(sess_b.join("rollout-bbb.jsonl"), "{\"m\":2}\n").unwrap();
        std::fs::write(
            b.join(".codex/session_index.jsonl"),
            "{\"id\":\"bbb\",\"thread_name\":\"B\",\"updated_at\":\"2026-05-01T09:00:00Z\"}\n",
        )
        .unwrap();

        let mut state_b = SyncState::default();
        let listing = store.list().unwrap();
        let report = apply(&b, &mut state_b, &store, &listing, &|| {}, &|_| {}).unwrap();
        assert_eq!(report.applied, 1); // A's session file landed
        assert!(b.join(".codex/sessions/2026/04/21/rollout-aaa.jsonl").exists());

        // B's index now lists BOTH sessions.
        let idx = std::fs::read_to_string(b.join(".codex/session_index.jsonl")).unwrap();
        assert!(idx.contains("\"aaa\""), "{idx}");
        assert!(idx.contains("\"bbb\""), "{idx}");
    }
}
