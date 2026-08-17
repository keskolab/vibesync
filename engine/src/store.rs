//! Sync stores: where the portable archive lives.
//!
//! Layout (identical across stores): `v1/files/<logical><codec-suffix>` for
//! content and `v1/files/<logical>.meta.json` sidecars. Content passes
//! through a [`Codec`](crate::codec::Codec) (gzip locally, gzip+age for any
//! third-party storage).
//!
//! - [`FolderStore`] — any directory: local testing, USB disks, and every
//!   cloud-synced folder (iCloud Drive, OneDrive, Dropbox, Google Drive)
//!   whose own client moves the bytes between machines.
//! - [`S3Store`] — S3-compatible object storage (Cloudflare R2, AWS S3).
//!   Presigned requests via rusty-s3, synchronous HTTP via ureq.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::codec::{Codec, GzipCodec};

const META_SUFFIX: &str = ".meta.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteMeta {
    /// SHA-256 of the *plaintext* content, hex.
    pub hash: String,
    pub mtime_ms: i64,
    pub size: u64,
    /// Machine that last pushed this file.
    pub source: String,
}

pub trait SyncStore: Send + Sync {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()>;
    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>>;
    fn list(&self) -> Result<Vec<(String, RemoteMeta)>>;
    /// Cheap credentials/reachability check — a single request, no
    /// enumeration (list() fetches per-object metadata and scales with store
    /// size). Returns whether the store already holds sync data.
    fn probe(&self) -> Result<bool> {
        Ok(!self.list()?.is_empty())
    }
    /// A handful of object keys, as cheaply as the backend allows.
    ///
    /// For probes that only need examples — "can this passphrase open
    /// anything here?" — never the whole store: `list()` also downloads a
    /// metadata sidecar per object, so on a real bucket it is ~10,000
    /// requests and takes minutes. Setup used to call it and looked hung.
    fn sample_keys(&self, max: usize) -> Result<Vec<String>> {
        let all = self.list()?;
        if all.is_empty() {
            return Ok(Vec::new());
        }
        // Spread across the listing, never the first N: objects cluster by
        // machine and by tool, so the front could be entirely one machine's
        // writes and give a confidently wrong verdict.
        let step = (all.len() / max).max(1);
        Ok(all.into_iter().step_by(step).take(max).map(|(k, _)| k).collect())
    }
}

// ---------------------------------------------------------------- folder

pub struct FolderStore {
    root: PathBuf,
    codec: Box<dyn Codec>,
}

impl FolderStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_codec(root, Box::new(GzipCodec))
    }

    pub fn with_codec(root: impl Into<PathBuf>, codec: Box<dyn Codec>) -> Self {
        Self { root: root.into(), codec }
    }

    fn files_root(&self) -> PathBuf {
        self.root.join("v1").join("files")
    }

    fn object_path(&self, logical: &str, suffix: &str) -> PathBuf {
        let mut p = self.files_root();
        for comp in logical.split('/') {
            p.push(comp);
        }
        let name = format!(
            "{}{}",
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            suffix
        );
        p.set_file_name(name);
        p
    }
}

impl SyncStore for FolderStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()> {
        let data_path = self.object_path(logical, self.codec.suffix());
        if let Some(parent) = data_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let encoded = self.codec.encode(plain)?;

        let tmp = data_path.with_extension("tmp");
        std::fs::write(&tmp, &encoded)?;
        std::fs::rename(&tmp, &data_path)?;

        let meta_path = self.object_path(logical, META_SUFFIX);
        let tmp = meta_path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
        std::fs::rename(&tmp, &meta_path)?;
        Ok(())
    }

    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>> {
        let data_path = self.object_path(logical, self.codec.suffix());
        let meta_path = self.object_path(logical, META_SUFFIX);
        if !data_path.exists() || !meta_path.exists() {
            return Ok(None);
        }
        let meta: RemoteMeta = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        let encoded = std::fs::read(&data_path)?;
        let plain = self
            .codec
            .decode(&encoded)
            .with_context(|| format!("decode {}", data_path.display()))?;
        Ok(Some((plain, meta)))
    }

    fn probe(&self) -> Result<bool> {
        let root = self.files_root();
        if !root.exists() {
            // Reachable but never synced to — verify we can actually write here.
            std::fs::create_dir_all(&root)?;
            return Ok(false);
        }
        Ok(walkdir::WalkDir::new(&root)
            .into_iter()
            .flatten()
            .any(|e| e.file_type().is_file()))
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
            let Some(base) = name.strip_suffix(META_SUFFIX) else {
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

// ---------------------------------------------------------------- s3 / r2

const SIGN_TTL: Duration = Duration::from_secs(3600);

pub struct S3Store {
    bucket: rusty_s3::Bucket,
    credentials: rusty_s3::Credentials,
    codec: Box<dyn Codec>,
    agent: ureq::Agent,
    /// Persisted listing cache: meta-object key -> (etag, RemoteMeta). The
    /// bucket LIST returns each object's ETag for free; a sidecar whose ETag
    /// is unchanged since last sync has unchanged content, so its meta needs
    /// no re-download. Turns a no-change listing from one GET per object
    /// into a handful of LIST pages.
    list_cache: Option<PathBuf>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ListCache {
    /// Which store this cache belongs to (endpoint|bucket) — discarded on
    /// mismatch so switching stores can't serve stale metadata.
    scope: String,
    entries: std::collections::HashMap<String, (String, RemoteMeta)>,
}

impl S3Store {
    /// `endpoint` e.g. `https://<account>.r2.cloudflarestorage.com`; region
    /// is `auto` for R2.
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
        codec: Box<dyn Codec>,
    ) -> Result<Self> {
        let endpoint: url::Url = endpoint.parse().context("invalid S3 endpoint URL")?;
        let bucket = rusty_s3::Bucket::new(
            endpoint,
            rusty_s3::UrlStyle::Path,
            bucket.to_string(),
            region.to_string(),
        )
        .context("invalid bucket config")?;
        let credentials =
            rusty_s3::Credentials::new(access_key_id.to_string(), secret_access_key.to_string());
        Ok(Self {
            bucket,
            credentials,
            codec,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(120))
                .max_idle_connections_per_host(32)
                .build(),
            list_cache: None,
        })
    }

    fn key(&self, logical: &str, suffix: &str) -> String {
        format!("v1/files/{logical}{suffix}")
    }

    fn put_bytes(&self, key: &str, body: &[u8]) -> Result<()> {
        use rusty_s3::S3Action;
        let action = self.bucket.put_object(Some(&self.credentials), key);
        let url = action.sign(SIGN_TTL);
        self.agent
            .put(url.as_str())
            .send_bytes(body)
            .map_err(|e| clean_s3_err("upload", e))?;
        Ok(())
    }

    /// All meta objects under `prefix` as (logical, etag), paginating.
    /// First `want` logical keys under `prefix` — one small LIST request,
    /// no sidecar downloads, no pagination.
    fn sample_prefix(&self, prefix: &str, want: usize) -> Result<Vec<String>> {
        use rusty_s3::S3Action;
        let root = "v1/files/";
        let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
        action.query_mut().insert("prefix", prefix);
        // Keys come in pairs (content + .meta.json sidecar) and only the
        // sidecar half names a logical object, so ask for extra.
        let max_keys = (want * 4).max(8).to_string();
        action.query_mut().insert("max-keys", &max_keys);
        let url = action.sign(SIGN_TTL);
        let text = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| clean_s3_err("list", e))?
            .into_string()?;
        let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
            .context("parse LIST response")?;
        let mut out = Vec::new();
        for obj in parsed.contents {
            let key = percent_encoding::percent_decode_str(&obj.key)
                .decode_utf8_lossy()
                .into_owned();
            if let Some(base) = key.strip_suffix(META_SUFFIX) {
                if let Some(logical) = base.strip_prefix(root) {
                    out.push(logical.to_string());
                    if out.len() >= want {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    fn list_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        use rusty_s3::S3Action;
        let root = "v1/files/";
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.query_mut().insert("prefix", prefix);
            if let Some(token) = &continuation {
                action.query_mut().insert("continuation-token", token.clone());
            }
            let url = action.sign(SIGN_TTL);
            let text = self
                .agent
                .get(url.as_str())
                .call()
                .map_err(|e| clean_s3_err("list", e))?
                .into_string()?;
            let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
                .context("parse LIST response")?;
            for obj in parsed.contents {
                // Keys arrive URL-encoded (encoding-type=url): v1%2Ffiles%2F...
                let key = percent_encoding::percent_decode_str(&obj.key)
                    .decode_utf8_lossy()
                    .into_owned();
                if let Some(base) = key.strip_suffix(META_SUFFIX) {
                    if let Some(logical) = base.strip_prefix(root) {
                        out.push((logical.to_string(), obj.etag.clone()));
                    }
                }
            }
            match parsed.next_continuation_token {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    /// One delimiter request: the tool namespaces under v1/files/ (e.g.
    /// claude/, vscode/, shared/) so they can be listed in parallel.
    fn top_prefixes(&self) -> Result<Vec<String>> {
        use rusty_s3::S3Action;
        let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
        action.query_mut().insert("prefix", "v1/files/");
        action.query_mut().insert("delimiter", "/");
        let url = action.sign(SIGN_TTL);
        let text = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| clean_s3_err("list", e))?
            .into_string()?;
        let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
            .context("parse LIST response")?;
        Ok(parsed
            .common_prefixes
            .into_iter()
            .map(|p| {
                percent_encoding::percent_decode_str(&p.prefix).decode_utf8_lossy().into_owned()
            })
            .collect())
    }

    /// Enable the persistent listing cache (app data dir file).
    pub fn with_list_cache(mut self, path: PathBuf) -> Self {
        self.list_cache = Some(path);
        self
    }

    fn cache_scope(&self) -> String {
        format!("{}|{}", self.bucket.base_url(), self.bucket.name())
    }

    fn load_list_cache(&self) -> ListCache {
        let Some(p) = &self.list_cache else { return ListCache::default() };
        let cache: ListCache = std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if cache.scope == self.cache_scope() {
            cache
        } else {
            ListCache::default()
        }
    }

    fn save_list_cache(&self, cache: &ListCache) {
        if let Some(p) = &self.list_cache {
            if let Ok(bytes) = serde_json::to_vec(cache) {
                let _ = std::fs::write(p, bytes);
            }
        }
    }

    /// get_bytes with retry/backoff — list() fetches thousands of meta
    /// sidecars; a transient failure must not silently shrink the listing
    /// (that made first pulls incomplete: 666 of 831 skills).
    fn get_bytes_retry(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut delay = std::time::Duration::from_millis(200);
        let mut last = None;
        for _ in 0..3 {
            match self.get_bytes(key) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    crate::dlog::warn(|| format!("store fetch retry for {key}: {e}"));
                    last = Some(e);
                    std::thread::sleep(delay);
                    delay *= 2;
                }
            }
        }
        Err(last.unwrap())
    }

    fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        use rusty_s3::S3Action;
        let action = self.bucket.get_object(Some(&self.credentials), key);
        let url = action.sign(SIGN_TTL);
        match self.agent.get(url.as_str()).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader().read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(clean_s3_err("download", e)),
        }
    }
}

/// Turn a ureq error into a human message WITHOUT the signed URL (which
/// contains the request signature). Surfaces the HTTP status meaningfully.
fn clean_s3_err(op: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(403, _) => anyhow::anyhow!(
            "Access denied (403) on {op}. Check that your API token is valid and has \
             Object Read & Write permission for this bucket."
        ),
        ureq::Error::Status(404, _) => {
            anyhow::anyhow!("Not found (404) on {op}. Check the bucket name.")
        }
        ureq::Error::Status(code, _) => anyhow::anyhow!("Storage returned HTTP {code} on {op}."),
        ureq::Error::Transport(t) => {
            anyhow::anyhow!("Could not reach storage on {op}: {}", t.kind())
        }
    }
}

impl SyncStore for S3Store {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()> {
        crate::dlog::debug(|| format!("uploading {logical} ({} KB)", plain.len() / 1024));
        let encoded = self.codec.encode(plain)?;
        self.put_bytes(&self.key(logical, self.codec.suffix()), &encoded)?;
        self.put_bytes(&self.key(logical, META_SUFFIX), &serde_json::to_vec(meta)?)?;
        Ok(())
    }

    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>> {
        crate::dlog::debug(|| format!("downloading {logical}"));
        let Some(meta_bytes) = self.get_bytes(&self.key(logical, META_SUFFIX))? else {
            return Ok(None);
        };
        let meta: RemoteMeta = serde_json::from_slice(&meta_bytes)?;
        let Some(encoded) = self.get_bytes(&self.key(logical, self.codec.suffix()))? else {
            return Ok(None);
        };
        let plain = self.codec.decode(&encoded).with_context(|| format!("decode {logical}"))?;
        Ok(Some((plain, meta)))
    }

    fn probe(&self) -> Result<bool> {
        use rusty_s3::S3Action;
        let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
        action.query_mut().insert("prefix", "v1/files/");
        action.query_mut().insert("max-keys", "1");
        let url = action.sign(SIGN_TTL);
        let text = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| clean_s3_err("list", e))?
            .into_string()?;
        let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&text)
            .context("parse LIST response")?;
        Ok(!parsed.contents.is_empty())
    }

    /// A few keys spread across the tool namespaces, in a handful of small
    /// requests — never the full `list()`, which also downloads a metadata
    /// sidecar per object (~10,000 requests on a real bucket, minutes long:
    /// setup called it and looked frozen).
    ///
    /// Namespaces are the cheap proxy for "different machines and different
    /// times": taking the first N keys of one prefix could sample nothing
    /// but a single machine's writes.
    fn sample_keys(&self, max: usize) -> Result<Vec<String>> {
        let prefixes = self.top_prefixes().unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        if prefixes.is_empty() {
            return self.sample_prefix("v1/files/", max);
        }
        // At least one from each namespace, topping up until we have `max`.
        let per = (max / prefixes.len()).max(1);
        for p in &prefixes {
            if out.len() >= max {
                break;
            }
            let want = per.min(max - out.len());
            match self.sample_prefix(p, want) {
                Ok(keys) => out.extend(keys),
                // One unreachable namespace must not sink the probe.
                Err(e) => crate::dlog::debug(|| format!("sample {p}: {e:#}")),
            }
        }
        Ok(out)
    }

    fn list(&self) -> Result<Vec<(String, RemoteMeta)>> {
        use rayon::prelude::*;
        // Namespaces paginate independently — list them concurrently instead
        // of one serial page walk across the whole store.
        let prefixes = self.top_prefixes().unwrap_or_default();
        let out: Vec<(String, String)> = if prefixes.is_empty() {
            self.list_prefix("v1/files/")?
        } else {
            prefixes
                .par_iter()
                .map(|p| self.list_prefix(p))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect()
        };
        // Serve unchanged sidecars from the ETag-validated cache; fetch only
        // the changed/unknown ones in parallel (serially this dominated sync).
        let mut cache = self.load_list_cache();
        let (hits, misses): (Vec<_>, Vec<_>) = out
            .into_iter()
            .partition(|(logical, etag)| {
                cache.entries.get(logical).map(|(e, _)| e == etag).unwrap_or(false)
            });
        crate::dlog::debug(|| {
            format!("s3 list: {} cached metas, {} to fetch", hits.len(), misses.len())
        });
        let mut result: Vec<(String, RemoteMeta)> = hits
            .iter()
            .map(|(logical, _)| (logical.clone(), cache.entries[logical].1.clone()))
            .collect();
        let fetched: Vec<(String, String, RemoteMeta)> = misses
            .par_iter()
            .map(|(logical, etag)| -> Result<Option<(String, String, RemoteMeta)>> {
                let Some(meta_bytes) = self.get_bytes_retry(&self.key(logical, META_SUFFIX))?
                else {
                    return Ok(None); // sidecar genuinely missing: interrupted upload
                };
                // Corrupt sidecar: skip rather than brick sync; source re-push heals.
                Ok(serde_json::from_slice::<RemoteMeta>(&meta_bytes)
                    .ok()
                    .map(|m| (logical.clone(), etag.clone(), m)))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        // Rebuild the cache from exactly this listing (drops deleted objects).
        let mut fresh = ListCache { scope: self.cache_scope(), entries: Default::default() };
        for (logical, etag) in &hits {
            fresh.entries.insert(logical.clone(), (etag.clone(), cache.entries[logical].1.clone()));
        }
        for (logical, etag, meta) in fetched {
            fresh.entries.insert(logical.clone(), (etag, meta.clone()));
            result.push((logical, meta));
        }
        cache = fresh;
        self.save_list_cache(&cache);
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

// Needed for get_bytes' read_to_end.
use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::AgeCodec;

    #[test]
    fn folder_store_age_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::with_codec(
            tmp.path(),
            Box::new(AgeCodec::with_work_factor("test-pass", 2)),
        );
        let meta = RemoteMeta { hash: "h".into(), mtime_ms: 1, size: 4, source: "m".into() };
        store.put("projects/${EHOME}-x/a.jsonl", b"data", &meta).unwrap();

        // Ciphertext on disk, not plaintext.
        let obj = tmp
            .path()
            .join("v1/files/projects/${EHOME}-x/a.jsonl.gz.age");
        let raw = std::fs::read(&obj).unwrap();
        assert!(!raw.windows(4).any(|w| w == b"data"));

        let (plain, got_meta) = store.get("projects/${EHOME}-x/a.jsonl").unwrap().unwrap();
        assert_eq!(plain, b"data");
        assert_eq!(got_meta, meta);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    /// Integration test against a real bucket; opt-in via env vars:
    /// VIBESYNC_TEST_S3_ENDPOINT / _REGION / _BUCKET / _ACCESS_KEY / _SECRET
    #[test]
    #[ignore]
    fn s3_store_roundtrip_live() {
        let var = |k: &str| std::env::var(format!("VIBESYNC_TEST_S3_{k}")).unwrap();
        let store = S3Store::new(
            &var("ENDPOINT"),
            &var("REGION"),
            &var("BUCKET"),
            &var("ACCESS_KEY"),
            &var("SECRET"),
            Box::new(AgeCodec::with_work_factor("live-test", 2)),
        )
        .unwrap();
        let meta = RemoteMeta { hash: "h".into(), mtime_ms: 1, size: 9, source: "ci".into() };
        store.put("vibesync-selftest/a.jsonl", b"live data", &meta).unwrap();
        let (plain, _) = store.get("vibesync-selftest/a.jsonl").unwrap().unwrap();
        assert_eq!(plain, b"live data");
        assert!(store
            .list()
            .unwrap()
            .iter()
            .any(|(l, _)| l == "vibesync-selftest/a.jsonl"));
    }
}

// ---------------------------------------------------------------- azure

/// Azure Blob Storage via a container SAS URL — the user pastes one string
/// (`https://<acct>.blob.core.windows.net/<container>?sv=...&sig=...`), no
/// account keys or signing code needed.
pub struct AzureSasStore {
    base: url::Url,
    codec: Box<dyn Codec>,
    agent: ureq::Agent,
}

impl AzureSasStore {
    pub fn new(container_sas_url: &str, codec: Box<dyn Codec>) -> Result<Self> {
        let base: url::Url = container_sas_url.parse().context("invalid Azure SAS URL")?;
        if base.query().unwrap_or("").is_empty() {
            anyhow::bail!("Azure URL is missing its SAS query (the part after '?')");
        }
        Ok(Self {
            base,
            codec,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(120))
                .max_idle_connections_per_host(32)
                .build(),
        })
    }

    fn blob_url(&self, key: &str) -> url::Url {
        let mut u = self.base.clone();
        {
            let mut segs = u.path_segments_mut().expect("base URL");
            for part in key.split('/') {
                segs.push(part);
            }
        }
        u
    }

    fn put_bytes(&self, key: &str, body: &[u8]) -> Result<()> {
        self.agent
            .put(self.blob_url(key).as_str())
            .set("x-ms-blob-type", "BlockBlob")
            .send_bytes(body)
            .with_context(|| format!("PUT {key}"))?;
        Ok(())
    }

    /// Same retry rationale as the S3 store: transient meta-fetch failures
    /// must not silently shrink the listing.
    fn get_bytes_retry(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut delay = std::time::Duration::from_millis(200);
        let mut last = None;
        for _ in 0..3 {
            match self.get_bytes(key) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    crate::dlog::warn(|| format!("store fetch retry for {key}: {e}"));
                    last = Some(e);
                    std::thread::sleep(delay);
                    delay *= 2;
                }
            }
        }
        Err(last.unwrap())
    }

    fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.agent.get(self.blob_url(key).as_str()).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader().read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(clean_s3_err("download", e)),
        }
    }
}

impl SyncStore for AzureSasStore {
    fn put(&self, logical: &str, plain: &[u8], meta: &RemoteMeta) -> Result<()> {
        let encoded = self.codec.encode(plain)?;
        self.put_bytes(&format!("v1/files/{logical}{}", self.codec.suffix()), &encoded)?;
        self.put_bytes(&format!("v1/files/{logical}{META_SUFFIX}"), &serde_json::to_vec(meta)?)?;
        Ok(())
    }

    fn get(&self, logical: &str) -> Result<Option<(Vec<u8>, RemoteMeta)>> {
        let Some(meta_bytes) = self.get_bytes(&format!("v1/files/{logical}{META_SUFFIX}"))? else {
            return Ok(None);
        };
        let meta: RemoteMeta = serde_json::from_slice(&meta_bytes)?;
        let Some(encoded) =
            self.get_bytes(&format!("v1/files/{logical}{}", self.codec.suffix()))?
        else {
            return Ok(None);
        };
        Ok(Some((self.codec.decode(&encoded)?, meta)))
    }

    fn probe(&self) -> Result<bool> {
        let mut u = self.base.clone();
        u.query_pairs_mut()
            .append_pair("restype", "container")
            .append_pair("comp", "list")
            .append_pair("prefix", "v1/files/")
            .append_pair("maxresults", "1");
        let xml = self.agent.get(u.as_str()).call().context("LIST blobs")?.into_string()?;
        Ok(xml.contains("<Name>"))
    }

    fn list(&self) -> Result<Vec<(String, RemoteMeta)>> {
        let mut names = Vec::new();
        let mut marker = String::new();
        loop {
            let mut u = self.base.clone();
            u.query_pairs_mut()
                .append_pair("restype", "container")
                .append_pair("comp", "list")
                .append_pair("prefix", "v1/files/");
            if !marker.is_empty() {
                u.query_pairs_mut().append_pair("marker", &marker);
            }
            let xml = self.agent.get(u.as_str()).call().context("LIST blobs")?.into_string()?;
            // Minimal XML scrape: blob names inside <Name>...</Name>.
            for part in xml.split("<Name>").skip(1) {
                if let Some(name) = part.split("</Name>").next() {
                    if let Some(base) = name.strip_suffix(META_SUFFIX) {
                        if let Some(logical) = base.strip_prefix("v1/files/") {
                            names.push(logical.to_string());
                        }
                    }
                }
            }
            marker = xml
                .split("<NextMarker>")
                .nth(1)
                .and_then(|s| s.split("</NextMarker>").next())
                .unwrap_or("")
                .to_string();
            if marker.is_empty() {
                break;
            }
        }
        use rayon::prelude::*;
        let mut out: Vec<(String, RemoteMeta)> = names
            .par_iter()
            .map(|logical| -> Result<Option<(String, RemoteMeta)>> {
                let Some(meta_bytes) =
                    self.get_bytes_retry(&format!("v1/files/{logical}{META_SUFFIX}"))?
                else {
                    return Ok(None);
                };
                Ok(serde_json::from_slice::<RemoteMeta>(&meta_bytes)
                    .ok()
                    .map(|m| (logical.clone(), m)))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}
