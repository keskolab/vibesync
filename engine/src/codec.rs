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

/// gzip → age (scrypt passphrase). The same passphrase on every machine.
pub struct AgeCodec {
    passphrase: String,
    /// scrypt work factor (log2). age's default is used in production;
    /// tests lower it to stay fast.
    work_factor: u8,
}

impl AgeCodec {
    pub fn new(passphrase: impl Into<String>) -> Self {
        Self { passphrase: passphrase.into(), work_factor: 18 }
    }

    #[doc(hidden)]
    pub fn with_work_factor(passphrase: impl Into<String>, work_factor: u8) -> Self {
        Self { passphrase: passphrase.into(), work_factor }
    }

    fn secret(&self) -> age::secrecy::SecretString {
        age::secrecy::SecretString::from(self.passphrase.clone())
    }
}

impl Codec for AgeCodec {
    fn encode(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let compressed = GzipCodec::compress(plain)?;
        let mut recipient = age::scrypt::Recipient::new(self.secret());
        recipient.set_work_factor(self.work_factor);
        age::encrypt(&recipient, &compressed).context("age encrypt")
    }

    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
        let identity = age::scrypt::Identity::new(self.secret());
        let compressed = age::decrypt(&identity, stored).context("age decrypt")?;
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
