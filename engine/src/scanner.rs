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

pub fn hash_file(path: &Path) -> Result<String> {
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

fn hex(bytes: &[u8]) -> String {
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
/// anywhere under the root. A root that is a single file yields one entry.
pub fn scan_root(
    abs_root: &Path,
    logical_prefix: &str,
    tok: &Tokenizer,
    exts: &[&str],
    exclude_dirs: &[&str],
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
