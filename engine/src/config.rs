//! Store configuration and factory.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::codec::{AgeCodec, Codec, GzipCodec};
use crate::store::{FolderStore, S3Store, SyncStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoreConfig {
    /// A directory: local, USB, or inside any cloud-synced folder.
    Folder {
        path: String,
        /// Encrypt content (mandatory when the folder is provider-synced).
        #[serde(default)]
        encrypted: bool,
    },
    /// S3-compatible object storage (Cloudflare R2: region = "auto").
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        access_key_id: String,
        /// Content is always encrypted on S3-class backends.
        secret_access_key: String,
    },
    /// Azure Blob Storage via a container SAS URL. Always encrypted.
    AzureSas { container_sas_url: String },
}

/// What a passphrase means for the data ALREADY in a storage location.
///
/// The passphrase is the only thing tying two machines together: the same
/// phrase derives the same key, a different phrase derives a different one.
/// Nothing at setup time used to verify it, so a second machine could be
/// configured with a mistyped or forgotten phrase, encrypt everything it
/// uploaded under a key no one else had, and only reveal it weeks later as
/// "sync is broken" (live incident 2026-08-17).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PassphraseCheck {
    /// No VibeSync data here yet — first machine, nothing to match against.
    NewStorage,
    /// Every sampled object opened: this phrase matches the existing data.
    Matches { sampled: usize },
    /// Nothing opened: wrong phrase for this storage.
    Mismatch { sampled: usize },
    /// Some objects open and some don't — this phrase is right, but the
    /// storage also holds objects written under a different one (another
    /// machine is misconfigured).
    Mixed { readable: usize, unreadable: usize },
    /// Couldn't tell (network, permissions). Never block setup on this.
    Inconclusive { reason: String },
}

/// Sample existing objects and try to decrypt them, so setup can say
/// "this is the right passphrase" before the user commits to it.
///
/// Sampling is spread across the listing rather than taken from the front:
/// objects cluster by machine and by namespace, so the first N could all
/// come from the very machine whose phrase is wrong.
pub fn check_passphrase(store: &dyn SyncStore) -> PassphraseCheck {
    const SAMPLE: usize = 6;

    // Cheap by design: one listing request, no metadata sidecars. Reading
    // the whole store here made setup sit on "Checking…" for minutes.
    let picks = match store.sample_keys(SAMPLE) {
        Ok(k) => k,
        Err(e) => return PassphraseCheck::Inconclusive { reason: format!("{e:#}") },
    };
    if picks.is_empty() {
        return PassphraseCheck::NewStorage;
    }

    let (mut readable, mut unreadable, mut other) = (0usize, 0usize, String::new());
    for logical in &picks {
        match store.get(logical) {
            Ok(Some(_)) => readable += 1,
            // Vanished between listing and fetch: tells us nothing.
            Ok(None) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                if crate::sync::is_undecryptable(&msg) {
                    unreadable += 1;
                } else if other.is_empty() {
                    other = msg;
                }
            }
        }
    }
    match (readable, unreadable) {
        (0, 0) => PassphraseCheck::Inconclusive {
            reason: if other.is_empty() { "no objects could be sampled".into() } else { other },
        },
        (r, 0) => PassphraseCheck::Matches { sampled: r },
        (0, u) => PassphraseCheck::Mismatch { sampled: u },
        (r, u) => PassphraseCheck::Mixed { readable: r, unreadable: u },
    }
}

/// Build the store described by `config`. `passphrase` is required whenever
/// content encryption applies (encrypted folders and all S3 backends).
pub fn open_store(config: &StoreConfig, passphrase: Option<&str>) -> Result<Box<dyn SyncStore>> {
    open_store_cached(config, passphrase, None)
}

/// `cache_dir`: where S3-class stores may persist their ETag-validated
/// listing cache (skips per-object meta downloads for unchanged objects).
pub fn open_store_cached(
    config: &StoreConfig,
    passphrase: Option<&str>,
    cache_dir: Option<&std::path::Path>,
) -> Result<Box<dyn SyncStore>> {
    let codec_for = |encrypted: bool| -> Result<Box<dyn Codec>> {
        if encrypted {
            let pass = passphrase
                .ok_or_else(|| anyhow::anyhow!("this store requires an encryption passphrase"))?;
            Ok(Box::new(AgeCodec::new(pass)))
        } else {
            Ok(Box::new(GzipCodec))
        }
    };
    match config {
        StoreConfig::Folder { path, encrypted } => {
            Ok(Box::new(FolderStore::with_codec(path.clone(), codec_for(*encrypted)?)))
        }
        StoreConfig::S3 { endpoint, region, bucket, access_key_id, secret_access_key } => {
            let mut s = S3Store::new(
                endpoint,
                region,
                bucket,
                access_key_id,
                secret_access_key,
                codec_for(true)?,
            )?;
            if let Some(dir) = cache_dir {
                s = s.with_list_cache(dir.join("store_list_cache.json"));
            }
            Ok(Box::new(s))
        }
        StoreConfig::AzureSas { container_sas_url } => Ok(Box::new(
            crate::store::AzureSasStore::new(container_sas_url, codec_for(true)?)?,
        )),
    }
}

/// A stable human-readable name for this machine (used in RemoteMeta.source).
pub fn machine_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_config_roundtrips_through_json() {
        let cfg = StoreConfig::Folder { path: "/tmp/x".into(), encrypted: true };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"type\":\"folder\""));
        let back: StoreConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StoreConfig::Folder { encrypted: true, .. }));
    }

    #[test]
    fn encrypted_store_requires_passphrase() {
        let cfg = StoreConfig::Folder { path: "/tmp/x".into(), encrypted: true };
        assert!(open_store(&cfg, None).is_err());
        assert!(open_store(&cfg, Some("pass")).is_ok());
    }

    #[test]
    fn machine_name_is_nonempty() {
        assert!(!machine_name().is_empty());
    }
}
