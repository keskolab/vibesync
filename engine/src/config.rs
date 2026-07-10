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
}

/// Build the store described by `config`. `passphrase` is required whenever
/// content encryption applies (encrypted folders and all S3 backends).
pub fn open_store(config: &StoreConfig, passphrase: Option<&str>) -> Result<Box<dyn SyncStore>> {
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
            Ok(Box::new(S3Store::new(
                endpoint,
                region,
                bucket,
                access_key_id,
                secret_access_key,
                codec_for(true)?,
            )?))
        }
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
