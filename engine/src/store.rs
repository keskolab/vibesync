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
    /// Fingerprint of the content key, `None` for unencrypted stores.
    fn key_id(&self) -> Option<String> {
        None
    }
    /// Can this machine's key open `logical`? Reads only the first few KB
    /// (the encryption header), never the whole object. `Ok(None)` = can't
    /// tell cheaply (unencrypted store, legacy object, vanished).
    fn opens(&self, _logical: &str) -> Result<Option<bool>> {
        Ok(None)
    }
}

/// How much of an object `opens` reads: an age header for one recipient is
/// a few hundred bytes.
const HEAD_BYTES: usize = 4096;

/// A temp-file name no other writer in this process can be using.
/// Parallel puts of two objects whose names differ only past the last dot
/// used to share one `.tmp` file and fail each other.
fn unique_tmp(path: &std::path::Path) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

/// Retry an upload on errors that are worth retrying (network, throttling,
/// server-side 5xx). A refusal like 403 won't change in a second.
#[allow(clippy::result_large_err)] // ureq's own error type
fn put_retry(
    op: impl Fn() -> std::result::Result<ureq::Response, ureq::Error>,
) -> std::result::Result<(), ureq::Error> {
    let mut delay = Duration::from_millis(250);
    let mut attempt = 0;
    loop {
        match op() {
            Ok(_) => return Ok(()),
            Err(e) => {
                let transient = match &e {
                    ureq::Error::Transport(_) => true,
                    ureq::Error::Status(code, _) => *code == 429 || *code >= 500,
                };
                attempt += 1;
                if !transient || attempt >= 3 {
                    return Err(e);
                }
                std::thread::sleep(delay);
                delay *= 2;
            }
        }
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

        // Data first, then the sidecar that announces it.
        write_atomic(&data_path, &encoded)?;
        write_atomic(&self.object_path(logical, META_SUFFIX), &serde_json::to_vec_pretty(meta)?)?;
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
        // The storage root itself must be readable — that failing IS the
        // store being unreachable. Below it, one bad entry is skipped like
        // the S3 listing skips a corrupt sidecar: a cloud-synced folder
        // routinely holds half-downloaded or offline placeholder files
        // (OneDrive Files On-Demand, evicted iCloud files), and one of them
        // used to fail every sync.
        std::fs::read_dir(&root).with_context(|| format!("read {}", root.display()))?;
        for entry in walkdir::WalkDir::new(&root) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    crate::dlog::warn(|| format!("store listing: skipping unreadable entry: {e}"));
                    continue;
                }
            };
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
            let meta = std::fs::read(entry.path())
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok(serde_json::from_slice::<RemoteMeta>(&b)?));
            match meta {
                Ok(meta) => out.push((logical, meta)),
                // Not counted as present: the writer's next push heals it.
                Err(e) => crate::dlog::warn(|| {
                    format!("store listing: skipping {logical} — its metadata can't be read ({e:#})")
                }),
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn key_id(&self) -> Option<String> {
        self.codec.key_id()
    }

    fn opens(&self, logical: &str) -> Result<Option<bool>> {
        use std::io::Read;
        let path = self.object_path(logical, self.codec.suffix());
        let mut head = Vec::with_capacity(HEAD_BYTES);
        match std::fs::File::open(&path) {
            Ok(f) => {
                f.take(HEAD_BYTES as u64).read_to_end(&mut head)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        Ok(self.codec.opens(&head))
    }
}

/// Write via a uniquely named temp file + rename, so a reader (or a
/// cloud-sync client) never sees a half-written object.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let tmp = unique_tmp(path);
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(res?)
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

    #[allow(clippy::result_large_err)] // the retried closure returns ureq's error
    fn put_bytes(&self, key: &str, body: &[u8]) -> Result<()> {
        use rusty_s3::S3Action;
        let action = self.bucket.put_object(Some(&self.credentials), key);
        let url = action.sign(SIGN_TTL);
        put_retry(|| self.agent.put(url.as_str()).send_bytes(body))
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

    fn key_id(&self) -> Option<String> {
        self.codec.key_id()
    }

    fn opens(&self, logical: &str) -> Result<Option<bool>> {
        use rusty_s3::S3Action;
        let key = self.key(logical, self.codec.suffix());
        let url = self.bucket.get_object(Some(&self.credentials), &key).sign(SIGN_TTL);
        // The presigned URL signs only the host, so a Range header is free
        // to add. A server ignoring it still only gets read up to HEAD_BYTES.
        match self.agent.get(url.as_str()).set("Range", &format!("bytes=0-{}", HEAD_BYTES - 1)).call() {
            Ok(resp) => Ok(self.codec.opens(&read_head(resp)?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(clean_s3_err("download", e)),
        }
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

fn read_head(resp: ureq::Response) -> Result<Vec<u8>> {
    let mut head = Vec::with_capacity(HEAD_BYTES);
    resp.into_reader().take(HEAD_BYTES as u64).read_to_end(&mut head)?;
    Ok(head)
}

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

    /// Many writers of the same object at once (duplicate scan keys, two
    /// syncs) used to share one `.tmp` file and fail each other.
    #[test]
    fn parallel_puts_of_one_object_all_succeed_and_leave_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path());
        std::thread::scope(|s| {
            for i in 0..16u8 {
                let store = &store;
                s.spawn(move || {
                    let body = vec![i; 1000];
                    let meta = RemoteMeta { hash: crate::scanner::hash_bytes(&body), mtime_ms: 1, size: 1000, source: "m".into() };
                    store.put("p/a.jsonl", &body, &meta).unwrap();
                });
            }
        });
        // Data and sidecar may come from different racing writers — the
        // reader's hash check (sync::pull_dir) is what guards that.
        let (plain, _) = store.get("p/a.jsonl").unwrap().unwrap();
        assert_eq!(plain.len(), 1000);
        assert!(plain.iter().all(|b| *b == plain[0]), "one writer's bytes, not a mix");
        let leftovers: Vec<_> = walkdir::WalkDir::new(tmp.path())
            .into_iter()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_unreadable_storage_root_is_an_error_not_an_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FolderStore::new(tmp.path());
        let meta = RemoteMeta { hash: "h".into(), mtime_ms: 1, size: 1, source: "m".into() };
        store.put("a", b"x", &meta).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let root = tmp.path().join("v1/files");
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
            let readable = std::fs::read_dir(&root).is_ok();
            let res = store.list();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            if !readable {
                // An empty listing here would read as "the store has nothing".
                assert!(res.is_err());
            }
        }
        // A missing root is simply a new store.
        assert!(FolderStore::new(tmp.path().join("nope")).list().unwrap().is_empty());
    }

    #[test]
    fn folder_probe_tells_own_key_from_foreign_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let mine = FolderStore::with_codec(tmp.path(), Box::new(AgeCodec::new("mine")));
        let meta = RemoteMeta { hash: "h".into(), mtime_ms: 1, size: 1, source: "m".into() };
        mine.put("a", b"x", &meta).unwrap();
        assert_eq!(mine.opens("a").unwrap(), Some(true));
        let theirs = FolderStore::with_codec(tmp.path(), Box::new(AgeCodec::new("theirs")));
        assert_eq!(theirs.opens("a").unwrap(), Some(false));
        assert_eq!(mine.opens("missing").unwrap(), None);
        assert_eq!(FolderStore::new(tmp.path()).key_id(), None);
    }

    #[test]
    fn xml_names_are_unescaped() {
        assert_eq!(xml_unescape("plain/name.json"), "plain/name.json");
        assert_eq!(xml_unescape("R&amp;D/&lt;x&gt;/&quot;q&quot;/it&apos;s"), "R&D/<x>/\"q\"/it's");
        // `&amp;lt;` is a literal "&lt;" in the name, not "<".
        assert_eq!(xml_unescape("a&amp;lt;b"), "a&lt;b");
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

    #[allow(clippy::result_large_err)] // the retried closure returns ureq's error
    fn put_bytes(&self, key: &str, body: &[u8]) -> Result<()> {
        let url = self.blob_url(key);
        put_retry(|| {
            self.agent.put(url.as_str()).set("x-ms-blob-type", "BlockBlob").send_bytes(body)
        })
        .map_err(|e| clean_azure_err("upload", e))?;
        Ok(())
    }

    /// One container LIST page. Errors are scrubbed like every other Azure
    /// call: ureq's own error text embeds the full request URL, and here
    /// that URL carries the SAS signature — a credential.
    fn list_page(&self, u: &url::Url) -> Result<String> {
        Ok(self.agent.get(u.as_str()).call().map_err(|e| clean_azure_err("list", e))?.into_string()?)
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
            Err(e) => Err(clean_azure_err("download", e)),
        }
    }
}

fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Azure's counterpart of `clean_s3_err`: a human message, never the URL.
fn clean_azure_err(op: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(403, _) => anyhow::anyhow!(
            "Access denied (403) on {op}. The SAS URL may have expired or lack \
             read/write/list permission for this container."
        ),
        ureq::Error::Status(404, _) => {
            anyhow::anyhow!("Not found (404) on {op}. Check the container name in the SAS URL.")
        }
        ureq::Error::Status(code, _) => anyhow::anyhow!("Storage returned HTTP {code} on {op}."),
        ureq::Error::Transport(t) => {
            anyhow::anyhow!("Could not reach storage on {op}: {}", t.kind())
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
        Ok(self.list_page(&u)?.contains("<Name>"))
    }

    fn key_id(&self) -> Option<String> {
        self.codec.key_id()
    }

    fn opens(&self, logical: &str) -> Result<Option<bool>> {
        let url = self.blob_url(&format!("v1/files/{logical}{}", self.codec.suffix()));
        match self.agent.get(url.as_str()).set("x-ms-range", &format!("bytes=0-{}", HEAD_BYTES - 1)).call() {
            Ok(resp) => Ok(self.codec.opens(&read_head(resp)?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(clean_azure_err("download", e)),
        }
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
            let xml = self.list_page(&u)?;
            // Minimal XML scrape: blob names inside <Name>...</Name>.
            for part in xml.split("<Name>").skip(1) {
                // Names are XML text: `R&D` arrives as `R&amp;D`, and taken
                // raw it named an object that doesn't exist.
                if let Some(name) = part.split("</Name>").next().map(xml_unescape) {
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
                .map(xml_unescape)
                .unwrap_or_default();
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
