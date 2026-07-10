//! Sync stores: where the portable archive lives.
//!
//! `FolderStore` is the v1 backend — any directory works, which covers local
//! testing, USB disks, and every cloud-synced folder (iCloud Drive, OneDrive,
//! Dropbox, Google Drive) whose own client moves the bytes between machines.
//!
//! Layout: `<root>/v1/files/<logical>.gz` + `<logical>.meta.json` sidecar.
//! Content is gzip-compressed; client-side encryption slots in as a codec
//! layer in a later milestone (before any cloud backend ships).

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteMeta {
    /// SHA-256 of the *uncompressed* content, hex.
    pub hash: String,
    pub mtime_ms: i64,
    pub size: u64,
    /// Machine that last pushed this file.
    pub source: String,
}

pub trait SyncStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()>;
    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>>;
    fn list(&self) -> Result<Vec<(String, RemoteMeta)>>;
}

pub struct FolderStore {
    root: PathBuf,
}

impl FolderStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn files_root(&self) -> PathBuf {
        self.root.join("v1").join("files")
    }

    fn data_path(&self, logical: &str) -> PathBuf {
        let mut p = self.files_root();
        for comp in logical.split('/') {
            p.push(comp);
        }
        p.set_extension(match p.extension().and_then(|e| e.to_str()) {
            Some(ext) => format!("{ext}.gz"),
            None => "gz".to_string(),
        });
        p
    }

    fn meta_path(&self, logical: &str) -> PathBuf {
        let mut p = self.files_root();
        for comp in logical.split('/') {
            p.push(comp);
        }
        let name = format!(
            "{}.meta.json",
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        );
        p.set_file_name(name);
        p
    }
}

impl SyncStore for FolderStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()> {
        let data_path = self.data_path(logical);
        if let Some(parent) = data_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(plain)?;
        let compressed = enc.finish()?;

        let tmp = data_path.with_extension("gz.tmp");
        std::fs::write(&tmp, &compressed)?;
        std::fs::rename(&tmp, &data_path)?;

        let meta_path = self.meta_path(logical);
        let tmp = meta_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
        std::fs::rename(&tmp, &meta_path)?;
        Ok(())
    }

    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>> {
        let data_path = self.data_path(logical);
        let meta_path = self.meta_path(logical);
        if !data_path.exists() || !meta_path.exists() {
            return Ok(None);
        }
        let meta: RemoteMeta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        let compressed = std::fs::read(&data_path)?;
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(&compressed[..])
            .read_to_end(&mut plain)
            .with_context(|| format!("decompress {}", data_path.display()))?;
        Ok(Some((plain, meta)))
    }

    fn list(&self) -> Result<Vec<(String, RemoteMeta)>> {
        let root = self.files_root();
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        for entry in walkdir::WalkDir::new(&root) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            let Some(base) = name.strip_suffix(".meta.json") else {
                continue;
            };
            let rel = entry.path().strip_prefix(&root)?;
            let mut comps: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let last = comps.last_mut().expect("file has a name");
            *last = base.to_string();
            let logical = comps.join("/");
            let meta: RemoteMeta = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            out.push((logical, meta));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}
