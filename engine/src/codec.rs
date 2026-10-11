//! Content codecs: how bytes are transformed before they reach a store.
//!
//! - `GzipCodec` — compression only. For folders the user picks: local and
//!   USB disks, and also cloud-synced folders (iCloud Drive, Dropbox, ...),
//!   which setup deliberately leaves unencrypted (README: "Where your data
//!   lives").
//! - `AgeCodec` — gzip, then age passphrase encryption (scrypt). Always used
//!   for S3/R2/Azure buckets: providers only ever see ciphertext. Note:
//!   object *names* and the `.meta.json` sidecars (content hash, size,
//!   mtime, machine name) are not encrypted.

use std::io::{Read, Write};

use anyhow::{Context, Result};

pub trait Codec: Send + Sync {
    fn encode(&self, plain: &[u8]) -> Result<Vec<u8>>;
    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>>;
    /// Suffix appended to object names, e.g. ".gz" or ".gz.age".
    fn suffix(&self) -> &'static str;
    /// Short fingerprint of the content key; `None` when nothing is
    /// encrypted. Never the key itself.
    fn key_id(&self) -> Option<String> {
        None
    }
    /// Given the first bytes of a stored object, can this codec open it?
    /// `None` = can't tell without the whole object or a full KDF run.
    fn opens(&self, _head: &[u8]) -> Option<bool> {
        Some(true)
    }
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

    fn key_id(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.keys().1.to_string().as_bytes());
        Some(crate::scanner::hex(&digest[..8]))
    }

    /// age unwraps the file key from the header alone (`decrypt` returns a
    /// lazy reader), so the first few KB are enough to tell "my key" from
    /// "someone else's" — no download, no decompression.
    fn opens(&self, head: &[u8]) -> Option<bool> {
        let Ok(dec) = age::Decryptor::new(head) else {
            // Not even a valid header: nobody can read this object.
            return Some(false);
        };
        if dec.is_scrypt() {
            // Legacy per-file passphrase mode: checking costs a full scrypt
            // run per object. Not worth it for a probe.
            return None;
        }
        let id: &dyn age::Identity = &self.keys().0;
        Some(dec.decrypt(std::iter::once(id)).is_ok())
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
    fn age_header_probe_tells_own_key_from_others() {
        let right = AgeCodec::new("right");
        let wrong = AgeCodec::new("wrong");
        // Incompressible, so the stored object really is larger than the head.
        let mut x = 0x2545_f491u32;
        let big: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let mine = right.encode(&big).unwrap();
        let theirs = wrong.encode(&big).unwrap();
        // Only the head is offered, as the stores do.
        assert_eq!(right.opens(&mine[..4096]), Some(true));
        assert_eq!(right.opens(&theirs[..4096]), Some(false));
        assert_eq!(right.opens(b"not an age file"), Some(false));
        assert_ne!(right.key_id(), wrong.key_id());
        assert_eq!(right.key_id(), AgeCodec::new("right").key_id());
        assert_eq!(GzipCodec.key_id(), None);
    }

    #[test]
    fn age_wrong_passphrase_fails() {
        let c = AgeCodec::with_work_factor("right", 2);
        let stored = c.encode(b"data").unwrap();
        let wrong = AgeCodec::with_work_factor("wrong", 2);
        assert!(wrong.decode(&stored).is_err());
    }
}
