//! Integration tests for the multi-repository support: MassIVE and iProX
//! listing via their (mocked) APIs, and the shared download engine validating
//! by size when a repository publishes no checksums and by MD5 when it does.

use std::collections::HashMap;
use std::io::{Read, Write as _};
use std::net::TcpListener;
use std::sync::Arc;

use indicatif::MultiProgress;
use pxdprowl::common::{ConcurrencyLimits, RemoteFile};
use pxdprowl::config::Config;
use pxdprowl::iprox::Iprox;
use pxdprowl::massive::Massive;
use pxdprowl::repository::{self, Repository};

// md5("hello")
const MD5_HELLO: &str = "5d41402abc4b2a76b9719d911017c592";
// md5 of something else, for the mismatch case
const MD5_WRONG: &str = "00000000000000000000000000000000";

const GNPS2_JSON: &str = r#"{
    "ok": true,
    "rows": [
        {"filepath": "raw/a.raw", "size": 5},
        {"filepath": "raw/dir/b.raw", "size": 3}
    ],
    "columns": ["filepath", "size"]
}"#;

/// Binds a local responder routing on the request path. Returns the port and
/// the accept-loop thread handle.
fn spawn_mock() -> (u16, std::thread::JoinHandle<()>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Hit counters for the rate-limit routes: first N requests 429, then 200.
    let limited_hits = Arc::new(AtomicUsize::new(0));
    let flaky_hits = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let limited_hits = limited_hits.clone();
            let flaky_hits = flaky_hits.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let path = target.split('?').next().unwrap_or("");
                // (response head, body bytes)
                let rate_limited = |hits: &Arc<AtomicUsize>, max_429: usize| {
                    if hits.fetch_add(1, Ordering::SeqCst) < max_429 {
                        (
                            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                            Vec::new(),
                        )
                    } else {
                        (
                            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_string(),
                            b"hello".to_vec(),
                        )
                    }
                };
                let (head, body): (String, Vec<u8>) = if path == "/dl/limited.raw" {
                    rate_limited(&limited_hits, 1)
                } else if path == "/dl/flaky.raw" {
                    // 429 for the first 3 hits: exhausts a max_retries=2 round,
                    // forcing the outer rate-limit retry round to kick in.
                    rate_limited(&flaky_hits, 3)
                } else if path == "/dl/chunked.raw" {
                    // No Content-Length (chunked), like MassIVE's servlet.
                    (
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_string(),
                        b"5\r\nhello\r\n0\r\n\r\n".to_vec(),
                    )
                } else {
                    let body = if path == "/iprox/datasets/PXD082693" {
                        format!(
                            r#"{{"dataFiles": [
                                {{"accession": "MS:1002846", "name": "Associated raw file URI", "value": "http://127.0.0.1:{port}/iprox/IPX0019033000/IPX0019033001/a.raw"}},
                                {{"accession": "MS:1002851", "name": "Other type file URI", "value": "http://127.0.0.1:{port}/iprox/IPX0019033000/IPX0019033001/metadata.xlsx"}}
                            ]}}"#
                        )
                    } else if path.starts_with("/gnps2") {
                        GNPS2_JSON.to_string()
                    } else if path == "/dl/a.raw" || path == "/dl/bad.raw" {
                        "hello".to_string()
                    } else if path == "/dl/dir/b.raw" {
                        "abc".to_string()
                    } else if path == "/iprox/IPX0019033000/IPX0019033001/a.raw" {
                        "hello".to_string()
                    } else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.flush();
                        return;
                    };
                    (
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                        body.into_bytes(),
                    )
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            });
        }
    });
    (port, handle)
}

fn test_config() -> Config {
    let mut config = Config::default();
    config.max_retries = 2;
    config
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pxdprowl-multi-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A repository with canned listing/checksums pointing at the mock server, so
/// the real download engine runs end to end without external network.
#[derive(Clone, Copy)]
struct StubRepo {
    files: &'static [RemoteFile],
    checksums: Option<&'static [(&'static str, &'static str)]>,
}

fn leak(files: Vec<RemoteFile>) -> &'static [RemoteFile] {
    Box::leak(files.into_boxed_slice())
}

impl Repository for StubRepo {
    async fn get_file_list(
        &self,
        _client: &reqwest::Client,
        _config: &Config,
        _accession: &str,
    ) -> anyhow::Result<Vec<RemoteFile>> {
        Ok(self.files.to_vec())
    }

    async fn fetch_checksums(
        &self,
        _client: &reqwest::Client,
        _config: &Config,
        _accession: &str,
    ) -> anyhow::Result<Option<HashMap<String, String>>> {
        Ok(self.checksums.map(|entries| {
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }))
    }
}

#[tokio::test]
async fn massive_listing_builds_raw_files_with_download_urls() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();

    let files = Massive
        .get_file_list_with(&client, &config, "MSV000079263", &format!("{base}/gnps2"), &format!("{base}/dl"))
        .await
        .unwrap();

    assert_eq!(files.len(), 2);
    assert_eq!(files[0].relative_path, "raw/a.raw");
    assert_eq!(files[0].name, "a.raw");
    assert_eq!(files[0].size, Some(5));
    assert_eq!(
        files[0].url,
        format!("{base}/dl?forceDownload=true&file=f.MSV000079263/raw/a.raw")
    );
    assert_eq!(files[1].relative_path, "raw/dir/b.raw");
    drop(server);
}

#[tokio::test]
async fn massive_listing_rejects_malformed_accession() {
    let client = reqwest::Client::new();
    let config = test_config();
    let err = Massive
        .get_file_list_with(&client, &config, "MSV'; DROP TABLE filename; --", "http://x", "http://y")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("invalid MassIVE accession"));
}

#[tokio::test]
async fn iprox_listing_keeps_only_raw_http_files() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();

    let files = Iprox
        .get_file_list_with(&client, &config, "PXD082693", &format!("{base}/iprox/datasets"))
        .await
        .unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative_path, "IPX0019033001/a.raw");
    assert_eq!(files[0].name, "a.raw");
    assert_eq!(
        files[0].url,
        format!("{base}/iprox/IPX0019033000/IPX0019033001/a.raw")
    );
    drop(server);
}

#[tokio::test]
async fn engine_downloads_to_subpaths_and_validates_by_size() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let limits = Arc::new(ConcurrencyLimits::new(&config));
    let multi = MultiProgress::new();
    let dir = temp_dir("size-ok");

    let files = leak(vec![
        RemoteFile {
            name: "a.raw".into(),
            relative_path: "raw/a.raw".into(),
            url: format!("{base}/dl/a.raw"),
            size: Some(5),
            category: Some("RAW".into()),
        },
        RemoteFile {
            name: "b.raw".into(),
            relative_path: "raw/dir/b.raw".into(),
            url: format!("{base}/dl/dir/b.raw"),
            size: Some(3),
            category: Some("RAW".into()),
        },
    ]);
    let repo = StubRepo {
        files,
        checksums: None,
    };
    let target = repository::DatasetTarget {
        repo_id: "PXD004584",
        label: "PXD004584",
        dest: &dir,
        gate: None,
    };
    let count = repository::download_dataset(repo, &client, &config, &limits, &target, &multi)
        .await
        .unwrap();

    assert_eq!(count, 2);
    assert_eq!(std::fs::read(dir.join("PXD004584/raw/a.raw")).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(dir.join("PXD004584/raw/dir/b.raw")).unwrap(),
        b"abc"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn engine_size_mismatch_fails_and_deletes_file() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let limits = Arc::new(ConcurrencyLimits::new(&config));
    let multi = MultiProgress::new();
    let dir = temp_dir("size-bad");

    // The listing claims 999 bytes but the server sends 5.
    let files = leak(vec![RemoteFile {
        name: "bad.raw".into(),
        relative_path: "raw/bad.raw".into(),
        url: format!("{base}/dl/bad.raw"),
        size: Some(999),
        category: Some("RAW".into()),
    }]);
    let repo = StubRepo {
        files,
        checksums: None,
    };
    let target = repository::DatasetTarget {
        repo_id: "PXD004584",
        label: "PXD004584",
        dest: &dir,
        gate: None,
    };
    let err = repository::download_dataset(repo, &client, &config, &limits, &target, &multi)
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("validation failed"), "err: {err:#}");
    assert!(!dir.join("PXD004584/raw/bad.raw").exists());
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn engine_validates_by_md5_when_checksums_published() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let limits = Arc::new(ConcurrencyLimits::new(&config));
    let multi = MultiProgress::new();
    let dir = temp_dir("md5-ok");

    let files = leak(vec![RemoteFile {
        name: "a.raw".into(),
        relative_path: "a.raw".into(),
        url: format!("{base}/dl/a.raw"),
        size: Some(5),
        category: Some("RAW".into()),
    }]);
    let repo = StubRepo {
        files,
        checksums: Some(&[("a.raw", MD5_HELLO)]),
    };
    let target = repository::DatasetTarget {
        repo_id: "PXD000001",
        label: "PXD000001",
        dest: &dir,
        gate: None,
    };
    let count = repository::download_dataset(repo, &client, &config, &limits, &target, &multi)
        .await
        .unwrap();

    assert_eq!(count, 1);
    assert_eq!(std::fs::read(dir.join("PXD000001/a.raw")).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn download_uses_listing_size_when_server_sends_no_content_length() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let multi = MultiProgress::new();
    let dir = temp_dir("chunked");
    let dest = dir.join("chunked.raw");

    // Chunked response (no Content-Length), like MassIVE's servlet: the
    // listing-reported size drives the progress bar and completeness check.
    repository::download_file(
        &client,
        &config,
        &format!("{base}/dl/chunked.raw"),
        &dest,
        &multi,
        Some(5),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn download_waits_out_429_and_succeeds_on_retry() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let multi = MultiProgress::new();
    let dir = temp_dir("limited");
    let dest = dir.join("limited.raw");

    // First attempt gets 429 (Retry-After: 0), the retry succeeds.
    repository::download_file(
        &client,
        &config,
        &format!("{base}/dl/limited.raw"),
        &dest,
        &multi,
        Some(5),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn download_survives_more_429s_than_max_retries_via_extra_rounds() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut config = test_config();
    config.max_retries = 2; // the first 3 429s exhaust this round; the outer round must save it
    let multi = MultiProgress::new();
    let dir = temp_dir("flaky");
    let dest = dir.join("flaky.raw");

    repository::download_file(
        &client,
        &config,
        &format!("{base}/dl/flaky.raw"),
        &dest,
        &multi,
        Some(5),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn engine_checksum_mismatch_fails_and_deletes_file() {
    let (port, server) = spawn_mock();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let config = test_config();
    let limits = Arc::new(ConcurrencyLimits::new(&config));
    let multi = MultiProgress::new();
    let dir = temp_dir("md5-bad");

    let files = leak(vec![RemoteFile {
        name: "a.raw".into(),
        relative_path: "a.raw".into(),
        url: format!("{base}/dl/a.raw"),
        size: Some(5),
        category: Some("RAW".into()),
    }]);
    let repo = StubRepo {
        files,
        checksums: Some(&[("a.raw", MD5_WRONG)]),
    };
    let target = repository::DatasetTarget {
        repo_id: "PXD000001",
        label: "PXD000001",
        dest: &dir,
        gate: None,
    };
    let err = repository::download_dataset(repo, &client, &config, &limits, &target, &multi)
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("validation failed"), "err: {err:#}");
    assert!(!dir.join("PXD000001/a.raw").exists());
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}
