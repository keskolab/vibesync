//! Cloud stores against a scripted local HTTP server: retries, error
//! hygiene (a SAS URL is a credential and must never reach a log line),
//! listing quirks, and the header-only readability probe.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use vibesync_engine::store::{AzureSasStore, RemoteMeta, S3Store, SyncStore};
use vibesync_engine::{AgeCodec, Codec, GzipCodec};

#[derive(Debug, Clone)]
struct Req {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Serves `script` in order (one response per connection, then closes);
/// once exhausted, answers 200 with an empty body. Records every request.
struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Req>>>,
}

fn mock(script: Vec<(u16, Vec<u8>)>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Req>>> = Default::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        let script = Mutex::new(script.into_iter());
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let target = parts.next().unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(Req { method, target, headers });
            let (code, body) = script.lock().unwrap().next().unwrap_or((200, Vec::new()));
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    Mock { base, seen }
}

impl Mock {
    fn requests(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }
}

fn meta() -> RemoteMeta {
    RemoteMeta { hash: "h".into(), mtime_ms: 1, size: 1, source: "m".into() }
}

fn s3(base: &str, codec: Box<dyn Codec>) -> S3Store {
    S3Store::new(base, "auto", "bucket", "AKIDEXAMPLE", "secret-key-value", codec).unwrap()
}

const SIG: &str = "secret-sig-value";

fn azure(base: &str, codec: Box<dyn Codec>) -> AzureSasStore {
    AzureSasStore::new(&format!("{base}/container?sv=2024-01-01&sp=rwl&sig={SIG}"), codec).unwrap()
}

/// A URL that refuses connections.
fn dead_url() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

// ------------------------------------------------------------------ uploads

#[test]
fn s3_upload_retries_a_server_error_then_succeeds() {
    let m = mock(vec![(500, vec![]), (200, vec![]), (200, vec![])]);
    s3(&m.base, Box::new(GzipCodec)).put("a.jsonl", b"x", &meta()).unwrap();
    let reqs = m.requests();
    assert_eq!(reqs.len(), 3, "{reqs:?}");
    assert!(reqs.iter().all(|r| r.method == "PUT"));
    assert_eq!(reqs[0].target, reqs[1].target, "the data object was retried");
}

#[test]
fn s3_upload_does_not_retry_a_refusal_and_hides_the_signature() {
    let m = mock(vec![(403, vec![])]);
    let err = format!("{:#}", s3(&m.base, Box::new(GzipCodec)).put("a.jsonl", b"x", &meta()).unwrap_err());
    assert_eq!(m.requests().len(), 1);
    assert!(err.contains("403"), "{err}");
    assert!(!err.contains("Signature") && !err.contains("secret-key-value"), "{err}");
}

#[test]
fn s3_upload_gives_up_after_three_server_errors() {
    let m = mock(vec![(503, vec![]), (503, vec![]), (503, vec![])]);
    assert!(s3(&m.base, Box::new(GzipCodec)).put("a.jsonl", b"x", &meta()).is_err());
    assert_eq!(m.requests().len(), 3);
}

#[test]
fn azure_upload_retries_and_never_shows_the_sas() {
    let m = mock(vec![(500, vec![]), (200, vec![]), (200, vec![])]);
    azure(&m.base, Box::new(GzipCodec)).put("a.jsonl", b"x", &meta()).unwrap();
    assert_eq!(m.requests().len(), 3);
    assert_eq!(m.requests()[0].header("x-ms-blob-type"), Some("BlockBlob"));

    let m = mock(vec![(403, vec![])]);
    let err = format!("{:#}", azure(&m.base, Box::new(GzipCodec)).put("a.jsonl", b"x", &meta()).unwrap_err());
    assert!(err.contains("403") && !err.contains(SIG) && !err.contains("sig="), "{err}");
}

/// The live leak: an offline laptop's sync error carried the whole SAS URL
/// into debug.log, the popover and a notification.
#[test]
fn azure_errors_never_contain_the_sas_on_any_path() {
    let dead = azure(&dead_url(), Box::new(GzipCodec));
    let errors = [
        format!("{:#}", dead.put("a.jsonl", b"x", &meta()).unwrap_err()),
        format!("{:#}", dead.list().unwrap_err()),
        format!("{:#}", dead.probe().unwrap_err()),
        format!("{:#}", dead.get("a.jsonl").unwrap_err()),
        format!("{:#}", dead.opens("a.jsonl").unwrap_err()),
    ];
    for e in &errors {
        assert!(!e.contains(SIG) && !e.contains("sig=") && !e.contains("sv="), "leaked: {e}");
    }
    let m = mock(vec![(500, vec![])]);
    let e = format!("{:#}", azure(&m.base, Box::new(GzipCodec)).list().unwrap_err());
    assert!(e.contains("500") && !e.contains(SIG), "{e}");
}

// ------------------------------------------------------------------ listing

#[test]
fn azure_listing_unescapes_names_and_follows_markers() {
    let meta_json = serde_json::to_vec(&meta()).unwrap();
    let page1 = b"<EnumerationResults><Blobs>\
        <Blob><Name>v1/files/R&amp;D/a.jsonl.meta.json</Name></Blob>\
        <Blob><Name>v1/files/R&amp;D/a.jsonl.gz</Name></Blob>\
        </Blobs><NextMarker>next&amp;page</NextMarker></EnumerationResults>"
        .to_vec();
    let page2 = b"<EnumerationResults><Blobs>\
        <Blob><Name>v1/files/b.jsonl.meta.json</Name></Blob>\
        </Blobs><NextMarker /></EnumerationResults>"
        .to_vec();
    let m = mock(vec![(200, page1), (200, page2), (200, meta_json.clone()), (200, meta_json)]);
    let listing = azure(&m.base, Box::new(GzipCodec)).list().unwrap();
    let keys: Vec<&str> = listing.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["R&D/a.jsonl", "b.jsonl"]);
    let reqs = m.requests();
    assert!(reqs[1].target.contains("marker=next%26page"), "{:?}", reqs[1].target);
    // The meta of the escaped name was fetched under its real name.
    assert!(reqs.iter().any(|r| r.target.contains("/v1/files/R&D/a.jsonl.meta.json?")), "{reqs:?}");
    assert!(!reqs.iter().any(|r| r.target.contains("&amp;")), "{reqs:?}");
}

// ------------------------------------------------------- readability probe

fn incompressible(n: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9u32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[test]
fn the_probe_reads_only_the_head_of_an_object() {
    let object = AgeCodec::new("right").encode(&incompressible(256 * 1024)).unwrap();

    // S3: a Range request; even a server that ignores it is read only so far.
    let m = mock(vec![(200, object.clone())]);
    let store = s3(&m.base, Box::new(AgeCodec::new("right")));
    assert_eq!(store.opens("a.jsonl").unwrap(), Some(true));
    assert_eq!(m.requests()[0].header("range"), Some("bytes=0-4095"));

    let m = mock(vec![(206, object[..4096].to_vec())]);
    let store = s3(&m.base, Box::new(AgeCodec::new("other")));
    assert_eq!(store.opens("a.jsonl").unwrap(), Some(false));

    // Azure uses its own range header.
    let m = mock(vec![(206, object[..4096].to_vec())]);
    let store = azure(&m.base, Box::new(AgeCodec::new("right")));
    assert_eq!(store.opens("a.jsonl").unwrap(), Some(true));
    assert_eq!(m.requests()[0].header("x-ms-range"), Some("bytes=0-4095"));

    // Vanished: can't tell, not an error.
    let m = mock(vec![(404, vec![])]);
    assert_eq!(s3(&m.base, Box::new(AgeCodec::new("right"))).opens("gone").unwrap(), None);
}

#[test]
fn unencrypted_stores_report_no_key() {
    let m = mock(vec![]);
    assert_eq!(s3(&m.base, Box::new(GzipCodec)).key_id(), None);
    assert!(s3(&m.base, Box::new(AgeCodec::new("p"))).key_id().is_some());
}
