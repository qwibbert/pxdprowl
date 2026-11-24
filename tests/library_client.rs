//! Integration tests for the high-level library API (`pxdprowl::Client`):
//! PXD resolution against a mocked ProteomeXchange PROXI API, and downloads
//! from a local loopback file server.

use std::io::{Read, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pxdprowl::config::{Config, DownloadRequest};
use pxdprowl::{Client, Repo};

const PRIDE_RECORD: &str = r#"{
    "title": "TMT spikes",
    "datasetSummary": {"hostingRepository": "PRIDE"},
    "identifiers": [
        {"accession": "MS:1001919", "name": "ProteomeXchange accession number", "value": "PXD000001"}
    ]
}"#;

const MASSIVE_RECORD: &str = r#"{
    "title": "Age-related changes in protein abundance in nematodes",
    "datasetSummary": {"hostingRepository": "MassIVE"},
    "identifiers": [
        {"accession": "MS:1002487", "name": "MassIVE dataset identifier", "value": "MSV000079263"},
        {"accession": "MS:1001919", "name": "ProteomeXchange accession number", "value": "PXD004584"}
    ]
}"#;

const JPOST_RECORD: &str = r#"{
    "title": "some jPOST dataset",
    "datasetSummary": {"hostingRepository": "jPOST"},
    "identifiers": [
        {"accession": "MS:1002632", "name": "jPOST dataset identifier", "value": "JPST001234567"},
        {"accession": "MS:1001919", "name": "ProteomeXchange accession number", "value": "PXD900001"}
    ]
}"#;

const UNKNOWN_BODY: &str = r#"{"error_code": "DatasetNotFound", "description": "no such dataset"}"#;

/// Binds a local responder routing on the request path: `/proxi/datasets/<acc>`
/// answers with a canned PROXI record, `/dl/<name>` serves a small file (the
/// `/dl/hello.raw` hit count is shared via `hello_hits`). Returns the port,
/// the hit counter and the accept-loop thread handle.
fn spawn_mock() -> (u16, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hello_hits = Arc::new(AtomicUsize::new(0));
    let hello_hits_in_loop = hello_hits.clone();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let hello_hits = hello_hits_in_loop.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let (head, body): (String, Vec<u8>) = match path.as_str() {
                    "/proxi/datasets/PXD000001" => (
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json".to_string(),
                        PRIDE_RECORD.as_bytes().to_vec(),
                    ),
                    "/proxi/datasets/PXD004584" => (
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json".to_string(),
                        MASSIVE_RECORD.as_bytes().to_vec(),
                    ),
                    "/proxi/datasets/PXD900001" => (
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json".to_string(),
                        JPOST_RECORD.as_bytes().to_vec(),
                    ),
                    p if p.starts_with("/proxi/datasets/") => (
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json".to_string(),
                        UNKNOWN_BODY.as_bytes().to_vec(),
                    ),
                    "/dl/hello.raw" => {
                        hello_hits.fetch_add(1, Ordering::SeqCst);
                        ("HTTP/1.1 200 OK".to_string(), b"hello".to_vec())
                    }
                    "/dl/abc.raw" => ("HTTP/1.1 200 OK".to_string(), b"abc".to_vec()),
                    _ => ("HTTP/1.1 404 Not Found".to_string(), b"not found".to_vec()),
                };
                let head = format!(
                    "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            });
        }
    });
    (port, hello_hits, handle)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pxdprowl-client-test-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_config(output: PathBuf) -> Config {
    let mut config = Config::default();
    config.output = output;
    config.max_retries = 2;
    config.update_check = false;
    config
}

fn client_for(port: u16, output: PathBuf) -> Client {
    Client::new(test_config(output))
        .unwrap()
        .with_proxi_base(format!("http://127.0.0.1:{port}/proxi"))
}

#[tokio::test]
async fn resolve_maps_pride_record_to_pride_repo() {
    let (port, _hits, server) = spawn_mock();
    let client = client_for(port, temp_dir("resolve-pride"));

    let resolved = client.resolve("PXD000001").await.unwrap();

    assert_eq!(resolved.accession, "PXD000001");
    assert_eq!(resolved.repo, Repo::Pride);
    assert_eq!(resolved.repo_id, "PXD000001");
    assert_eq!(resolved.info.title.as_deref(), Some("TMT spikes"));
    drop(server);
}

#[tokio::test]
async fn resolve_maps_massive_record_to_its_msv_id() {
    let (port, _hits, server) = spawn_mock();
    let client = client_for(port, temp_dir("resolve-massive"));

    let resolved = client.resolve("PXD004584").await.unwrap();

    assert_eq!(resolved.repo, Repo::Massive);
    // The repository-local id is the MSV... id, not the PXD accession.
    assert_eq!(resolved.repo_id, "MSV000079263");
    drop(server);
}

#[tokio::test]
async fn resolve_rejects_unsupported_hosting_repository() {
    let (port, _hits, server) = spawn_mock();
    let client = client_for(port, temp_dir("resolve-jpost"));

    let err = client.resolve("PXD900001").await.unwrap_err();

    assert!(format!("{err:#}").contains("jPOST"), "err: {err:#}");
    drop(server);
}

#[tokio::test]
async fn resolve_rejects_unknown_accession() {
    let (port, _hits, server) = spawn_mock();
    let client = client_for(port, temp_dir("resolve-unknown"));

    let err = client.resolve("PXD999999999").await.unwrap_err();

    assert!(format!("{err:#}").contains("PXD999999999"), "err: {err:#}");
    drop(server);
}

#[tokio::test]
async fn resolve_rejects_malformed_accession_without_a_request() {
    // Port 9 (discard) would fail any actual request; validation must fire first.
    let client = client_for(9, temp_dir("resolve-bad"));
    let err = client.resolve("not an accession").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid accession"),
        "err: {err:#}"
    );
}

#[tokio::test]
async fn download_url_fetches_a_loopback_file_into_the_output_dir() {
    let (port, _hits, server) = spawn_mock();
    let dir = temp_dir("url");
    let client = client_for(port, dir.clone());

    let dest = client
        .download_url(&format!("http://127.0.0.1:{port}/dl/hello.raw"))
        .await
        .unwrap();

    assert_eq!(dest, dir.join("hello.raw"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn download_runs_a_batch_and_deduplicates_repeated_requests() {
    let (port, hello_hits, server) = spawn_mock();
    let dir = temp_dir("batch");
    let client = client_for(port, dir.clone());

    client
        .download(&[
            DownloadRequest::Url(format!("http://127.0.0.1:{port}/dl/hello.raw")),
            DownloadRequest::Url(format!("http://127.0.0.1:{port}/dl/abc.raw")),
            // Duplicate of the first request: must be skipped, not run twice.
            DownloadRequest::Url(format!("http://127.0.0.1:{port}/dl/hello.raw")),
        ])
        .await
        .unwrap();

    assert_eq!(std::fs::read(dir.join("hello.raw")).unwrap(), b"hello");
    assert_eq!(std::fs::read(dir.join("abc.raw")).unwrap(), b"abc");
    assert_eq!(
        hello_hits.load(Ordering::SeqCst),
        1,
        "duplicate was not skipped"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}

#[tokio::test]
async fn download_summarizes_failed_requests() {
    let (port, _hits, server) = spawn_mock();
    let dir = temp_dir("batch-fail");
    let client = client_for(port, dir.clone());

    let err = client
        .download(&[
            DownloadRequest::Url(format!("http://127.0.0.1:{port}/dl/hello.raw")),
            DownloadRequest::Url(format!("http://127.0.0.1:{port}/dl/missing.raw")),
        ])
        .await
        .unwrap_err();

    let msg = format!("{err:#}");
    assert!(msg.contains("1 download(s) failed"), "msg: {msg}");
    assert!(msg.contains("missing.raw"), "msg: {msg}");
    // The successful request still ran.
    assert_eq!(std::fs::read(dir.join("hello.raw")).unwrap(), b"hello");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(server);
}
