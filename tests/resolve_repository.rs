//! Integration test: PXD accessions are resolved against a (mocked)
//! ProteomeXchange PROXI API before any repository downloader is chosen.
//!
//! A local responder serves canned PROXI records per accession; the resolver
//! is pointed at it via `resolve_with_base`.

use std::io::{Read, Write as _};
use std::net::TcpListener;

use pxdprowl::config::Config;
use pxdprowl::proteomexchange::{self, HostingRepository};

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

const EMBARGOED_BODY: &str =
    r#"{"error_code": "DatasetNotYetReleased", "repository": "PRIDE", "description": "embargoed"}"#;
const UNKNOWN_BODY: &str = r#"{"error_code": "DatasetNotFound", "description": "no such dataset"}"#;

/// Binds a local responder that routes on the request path: `/datasets/<acc>`
/// answers with the canned record or 404 body. Returns the port and the
/// accept-loop thread handle.
fn spawn_proxi_mock() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let (status, reason, body) = match path.as_str() {
                    "/datasets/PXD000001" => (200, "OK", PRIDE_RECORD.to_string()),
                    "/datasets/PXD004584" => (200, "OK", MASSIVE_RECORD.to_string()),
                    "/datasets/PXD900001" => (200, "OK", JPOST_RECORD.to_string()),
                    "/datasets/PXD082740" => (404, "Not Found", EMBARGOED_BODY.to_string()),
                    "/datasets/PXD999999999" => (404, "Not Found", UNKNOWN_BODY.to_string()),
                    _ => (404, "Not Found", UNKNOWN_BODY.to_string()),
                };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
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

#[tokio::test]
async fn resolves_pride_dataset_as_supported() {
    let (port, server) = spawn_proxi_mock();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    let info = proteomexchange::resolve_with_base(&client, &test_config(), "PXD000001", &base)
        .await
        .unwrap();

    assert_eq!(info.hosting_repository(), HostingRepository::Pride);
    assert!(info.hosting_repository().is_supported());
    assert_eq!(info.repository_id(), None);
    assert!(proteomexchange::ensure_supported("PXD000001", &info.hosting_repository()).is_ok());
    drop(server);
}

#[tokio::test]
async fn resolves_massive_dataset_with_repo_id_and_supported() {
    let (port, server) = spawn_proxi_mock();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    let info = proteomexchange::resolve_with_base(&client, &test_config(), "PXD004584", &base)
        .await
        .unwrap();

    assert_eq!(info.hosting_repository(), HostingRepository::Massive);
    assert_eq!(info.repository_id(), Some("MSV000079263"));
    assert!(proteomexchange::ensure_supported("PXD004584", &info.hosting_repository()).is_ok());
    drop(server);
}

#[tokio::test]
async fn unsupported_repository_still_errors_with_name() {
    let (port, server) = spawn_proxi_mock();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    let info = proteomexchange::resolve_with_base(&client, &test_config(), "PXD900001", &base)
        .await
        .unwrap();

    assert_eq!(info.hosting_repository(), HostingRepository::Jpost);
    assert_eq!(info.repository_id(), Some("JPST001234567"));
    let err = proteomexchange::ensure_supported("PXD900001", &info.hosting_repository())
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("PXD900001"), "message was: {msg}");
    assert!(msg.contains("jPOST"), "message was: {msg}");
    drop(server);
}

#[tokio::test]
async fn embargoed_accession_errors_with_repository() {
    let (port, server) = spawn_proxi_mock();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    let err = proteomexchange::resolve_with_base(&client, &test_config(), "PXD082740", &base)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("PXD082740"), "message was: {msg}");
    assert!(msg.contains("not yet released"), "message was: {msg}");
    assert!(msg.contains("PRIDE"), "message was: {msg}");
    drop(server);
}

#[tokio::test]
async fn unknown_accession_errors_as_404() {
    let (port, server) = spawn_proxi_mock();
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");

    let err = proteomexchange::resolve_with_base(&client, &test_config(), "PXD999999999", &base)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("PXD999999999"), "message was: {msg}");
    assert!(msg.contains("no public record"), "message was: {msg}");
    drop(server);
}
