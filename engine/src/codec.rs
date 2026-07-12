//! Content codecs: how bytes are transformed before they reach a store.
//!
//! - `GzipCodec` — compression only. For local folders and USB disks where
//!   the disk itself is under the user's control.
//! - `AgeCodec` — gzip, then age passphrase encryption (scrypt). Mandatory
//!   for any third-party storage (cloud folders, S3/R2): providers only ever
//!   see ciphertext. Note: object *names* still reveal logical paths
//!   (project dir names); content, titles, and code never leave in plaintext.

use std::io::{Read, Write};

use anyhow::{Context, Result};

pub trait Codec: Send + Sync {
    fn encode(&self, plain: &[u8]) -> Result<Vec<u8>>;
    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>>;
    /// Suffix appended to object names, e.g. ".gz" or ".gz.age".
    fn suffix(&self) -> &'static str;
}

pub struct GzipCodec;

impl GzipCodec {
    fn compress(plain: &[u8]) -> Result<Vec<u8>> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(plain)?;
        Ok(enc.finish()?)
    }

    fn decompress(stored: &[u8]) -> Result<Vec<u8>> {
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(stored)
            .read_to_end(&mut plain)
            .context("gzip decompress")?;
        Ok(plain)
    }
}

impl Codec for GzipCodec {
    fn encode(&self, plain: &[u8]) -> Result<Vec<u8>> {
        Self::compress(plain)
    }

    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
        Self::decompress(stored)
    }

    fn suffix(&self) -> &'static str {
        ".gz"
    }
}

/// gzip → age. The passphrase is stretched (scrypt) into an X25519 key
/// ONCE when the codec is built; every file is then encrypted against that
/// key with cheap public-key operations. Same passphrase on every machine
/// derives the same key. Old objects encrypted in age's per-file passphrase
/// mode still decrypt via a fallback path.
pub struct AgeCodec {
    passphrase: String,
    /// Derived lazily on first encrypt/decrypt — deriving eagerly made
    /// opening a store (and "Test connection") pay the full KDF cost.
    keys: std::sync::OnceLock<(age::x25519::Identity, age::x25519::Recipient)>,
}

impl AgeCodec {
    pub fn new(passphrase: impl Into<String>) -> Self {
        Self { passphrase: passphrase.into(), keys: std::sync::OnceLock::new() }
    }

    fn keys(&self) -> &(age::x25519::Identity, age::x25519::Recipient) {
        self.keys.get_or_init(|| {
            let identity = derive_identity(&self.passphrase);
            let recipient = identity.to_public();
            (identity, recipient)
        })
    }

    #[doc(hidden)]
    pub fn with_work_factor(passphrase: impl Into<String>, _work_factor: u8) -> Self {
        Self::new(passphrase)
    }
}

/// scrypt output per passphrase, process-wide. The KDF is deliberately
/// expensive (~1s of CPU) and a fresh codec is built for EVERY sync — the
/// per-instance OnceLock made every autosync pay it again. Derive once per
/// passphrase per app run.
static DERIVED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, [u8; 32]>>,
> = std::sync::LazyLock::new(Default::default);

/// passphrase -> scrypt(fixed salt) -> 32 bytes -> bech32 AGE-SECRET-KEY.
/// Deterministic so every machine derives the same key.
fn derive_identity(passphrase: &str) -> age::x25519::Identity {
    use bech32::{ToBase32, Variant};
    let key: [u8; 32] = {
        let mut cache = DERIVED.lock().unwrap();
        match cache.get(passphrase) {
            Some(k) => *k,
            None => {
                let mut key = [0u8; 32];
                let params = scrypt::Params::new(17, 8, 1, 32).expect("scrypt params");
                scrypt::scrypt(passphrase.as_bytes(), b"vibesync-age-v1", &params, &mut key)
                    .expect("scrypt");
                cache.insert(passphrase.to_string(), key);
                key
            }
        }
    };
    let encoded = bech32::encode("age-secret-key-", key.to_base32(), Variant::Bech32)
        .expect("bech32");
    encoded.to_uppercase().parse().expect("valid derived age identity")
}

impl Codec for AgeCodec {
    fn encode(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let compressed = GzipCodec::compress(plain)?;
        age::encrypt(&self.keys().1, &compressed).context("age encrypt")
    }

    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
        let compressed = match age::decrypt(&self.keys().0, stored) {
            Ok(c) => c,
            Err(_) => {
                // Legacy objects: age passphrase (scrypt) mode.
                let secret = age::secrecy::SecretString::from(self.passphrase.clone());
                age::decrypt(&age::scrypt::Identity::new(secret), stored)
                    .context("age decrypt (both key modes failed)")?
            }
        };
        GzipCodec::decompress(&compressed)
    }

    fn suffix(&self) -> &'static str {
        ".gz.age"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_roundtrip() {
        let c = GzipCodec;
        let data = b"{\"line\":1}\n{\"line\":2}\n".repeat(100);
        let stored = c.encode(&data).unwrap();
        assert!(stored.len() < data.len());
        assert_eq!(c.decode(&stored).unwrap(), data);
    }

    #[test]
    fn age_roundtrip() {
        let c = AgeCodec::with_work_factor("correct horse battery staple", 2);
        let data = b"secret session content".to_vec();
        let stored = c.encode(&data).unwrap();
        assert_ne!(stored, data);
        assert_eq!(c.decode(&stored).unwrap(), data);
    }

    #[test]
    fn age_wrong_passphrase_fails() {
        let c = AgeCodec::with_work_factor("right", 2);
        let stored = c.encode(b"data").unwrap();
        let wrong = AgeCodec::with_work_factor("wrong", 2);
        assert!(wrong.decode(&stored).is_err());
    }
}
