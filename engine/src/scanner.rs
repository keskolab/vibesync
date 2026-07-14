//! Walks tool storage roots and produces logical-path-keyed file entries.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tokenizer::Tokenizer;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// Machine-independent path, e.g. `projects/${EHOME}-dev-proj/<uuid>.jsonl`.
    pub logical: String,
    /// Absolute path on this machine.
    pub abs: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
    /// SHA-256 of the file content, hex.
    pub hash: String,
}

/// Process-lifetime hash cache: (mtime_ms, size) -> hash per absolute path.
/// Scans re-hash every file on every sync; for a long-running tray app the
/// overwhelming majority are unchanged between 15-minute autosyncs, so a
/// stat() replaces a full read+SHA. Validated by mtime+size, so an edited
/// file always re-hashes; bounded by the fleet's file count (a few MB).
static HASH_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, (i64, u64, String)>>,
> = std::sync::LazyLock::new(Default::default);

/// Aggregated hashing work since the last take — lets the app's debug log
/// separate "traversal was slow" from "reading/hashing files was slow"
/// (the latter usually means antivirus on-access scanning).
#[derive(Debug, Default, Clone)]
pub struct HashStats {
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub hash_ms: u64,
    pub cache_hits: u64,
    /// Worst single file: (path, ms).
    pub slowest: Option<(std::path::PathBuf, u64)>,
}

static HASH_STATS: std::sync::LazyLock<std::sync::Mutex<HashStats>> =
    std::sync::LazyLock::new(Default::default);

static HASH_CACHE_FILE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Persist the hash cache across app launches: without this, every fresh
/// process re-reads every file once (a 2-minute Defender-throttled ordeal on
/// the Windows fleet). Loads existing entries on first call.
pub fn set_hash_cache_file(path: std::path::PathBuf) {
    let mut file = HASH_CACHE_FILE.lock().unwrap();
    if file.as_ref() == Some(&path) {
        return;
    }
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(entries) =
            serde_json::from_slice::<Vec<(std::path::PathBuf, i64, u64, String)>>(&bytes)
        {
            let mut cache = HASH_CACHE.lock().unwrap();
            for (p, m, sz, h) in entries {
                cache.entry(p).or_insert((m, sz, h));
            }
        }
    }
    *file = Some(path);
}

/// Write the cache back (called at sync end). Entries for files that no
/// longer exist are dropped so the file can't grow without bound.
pub fn save_hash_cache() {
    let Some(path) = HASH_CACHE_FILE.lock().unwrap().clone() else { return };
    let cache = HASH_CACHE.lock().unwrap();
    let entries: Vec<(&std::path::PathBuf, i64, u64, &String)> = cache
        .iter()
        .filter(|(p, _)| p.exists())
        .map(|(p, (m, s, h))| (p, *m, *s, h))
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&entries) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// Return and reset the accumulated stats.
pub fn take_hash_stats() -> HashStats {
    std::mem::take(&mut HASH_STATS.lock().unwrap())
}

pub fn hash_file(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path)?;
    let key = (mtime_ms(path).unwrap_or(0), meta.len());
    if let Some((m, s, h)) = HASH_CACHE.lock().unwrap().get(path) {
        if (*m, *s) == key {
            HASH_STATS.lock().unwrap().cache_hits += 1;
            return Ok(h.clone());
        }
    }
    crate::dlog::debug(|| {
        format!("reading {} ({} KB)", path.display(), key.1 / 1024)
    });
    let t = std::time::Instant::now();
    let hash = hash_file_uncached(path)?;
    let ms = t.elapsed().as_millis() as u64;
    {
        let mut st = HASH_STATS.lock().unwrap();
        st.files_hashed += 1;
        st.bytes_hashed += key.1;
        st.hash_ms += ms;
        if st.slowest.as_ref().map(|(_, m)| ms > *m).unwrap_or(true) {
            st.slowest = Some((path.to_path_buf(), ms));
        }
    }
    if ms > 500 {
        crate::dlog::warn(|| {
            format!("slow file read/hash: {} took {ms} ms ({} KB)", path.display(), key.1 / 1024)
        });
    }
    HASH_CACHE.lock().unwrap().insert(path.to_path_buf(), (key.0, key.1, hash.clone()));
    Ok(hash)
}

fn hash_file_uncached(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hash_bytes(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn mtime_ms(path: &Path) -> Result<i64> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    Ok(mtime.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0))
}

/// Scan `abs_root` recursively for matching files, producing entries whose
/// logical paths start with `logical_prefix` and have every path component
/// tokenized (so encoded-home directory names become portable).
///
/// `exts` empty = all files. `exclude_dirs` are directory names skipped
/// anywhere under the root; `exclude_files` are file names (or `*.suffix`
/// patterns) skipped anywhere. A root that is a single file yields one entry.
pub fn scan_root(
    abs_root: &Path,
    logical_prefix: &str,
    tok: &Tokenizer,
    exts: &[&str],
    exclude_dirs: &[&str],
    exclude_files: &[&str],
) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    if !abs_root.exists() {
        return Ok(out);
    }
    if abs_root.is_file() {
        let meta = std::fs::metadata(abs_root)?;
        let name = abs_root.file_name().unwrap_or_default().to_string_lossy();
        out.push(FileEntry {
            logical: format!("{logical_prefix}/{name}"),
            abs: abs_root.to_path_buf(),
            size: meta.len(),
            mtime_ms: mtime_ms(abs_root)?,
            hash: hash_file(abs_root)?,
        });
        return Ok(out);
    }
    let walker = walkdir::WalkDir::new(abs_root).follow_links(false).into_iter();
    for entry in walker.filter_entry(|e| {
        !(e.file_type().is_dir()
            && e.file_name()
                .to_str()
                .map(|n| exclude_dirs.iter().any(|x| x.eq_ignore_ascii_case(n)))
                .unwrap_or(false))
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| crate::adapters::file_excluded(n, exclude_files))
            .unwrap_or(false)
        {
            continue;
        }
        let ext_ok = exts.is_empty()
            || path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        let rel = path.strip_prefix(abs_root)?;
        let mut logical = String::from(logical_prefix);
        for comp in rel.components() {
            let comp = comp.as_os_str().to_string_lossy();
            logical.push('/');
            logical.push_str(&tok.tokenize_encoded(&comp));
        }
        let meta = entry.metadata()?;
        out.push(FileEntry {
            logical,
            abs: path.to_path_buf(),
            size: meta.len(),
            mtime_ms: mtime_ms(path)?,
            hash: hash_file(path)?,
        });
    }
    out.sort_by(|a, b| a.logical.cmp(&b.logical));
    Ok(out)
}
