//! [iProX](https://www.iprox.org) implementation of [`Repository`].
//!
//! iProX publishes no per-file checksums, so downloads are validated by size
//! when known (the download itself is checked against `Content-Length`).
//! File lists come from iProX's own PROXI endpoint (keyed by the PXD
//! accession); files are downloaded over HTTPS from `download.iprox.cn`
//! (URLs recorded as `download.iprox.org` are rewritten: that host's
//! certificate is only valid for `*.iprox.cn`). The server supports `Range`
//! requests for resume. Only `Associated raw file URI` entries are listed as
//! RAW files.
//!
//! PROXI reports no file sizes, so after listing, each file's size is
//! discovered with a HEAD request (`Content-Length`) — exact, unlike the
//! website's own file-size metadata, which is only approximate — and used
//! for the `files` listing, the progress bars, and size validation.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use log::debug;
use serde::Deserialize;
use tokio::sync::Semaphore;

use super::common::{self, RemoteFile};
use super::config::Config;
use super::repository::Repository;

/// iProX PROXI API base, keyed by PXD accession.
pub const IPROX_PROXI_BASE: &str = "https://www.iprox.cn/proxi/datasets";

/// Total timeout for iProX PROXI calls. The endpoint is very slow — observed
/// 47–93 s for a single dataset record — so the shared [`common::API_TIMEOUT`]
/// (60 s) times out on roughly half of all requests. Use a larger budget for
/// iProX only; the other repositories answer in well under a second.
pub const IPROX_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// cvParam accession of "Associated raw file URI".
const RAW_FILE_URI_ACCESSION: &str = "MS:1002846";

/// Maximum number of concurrent HEAD requests used to discover file sizes.
const HEAD_CONCURRENCY: usize = 16;

/// The [iProX](https://www.iprox.org) repository.
#[derive(Debug, Clone, Copy)]
pub struct Iprox;

/// An iProX PROXI dataset record (only the fields pxdprowl needs).
#[derive(Debug, Deserialize)]
struct IproxRecord {
    #[serde(default, rename = "dataFiles")]
    data_files: Vec<IproxFile>,
}

/// One `dataFiles` entry of an [`IproxRecord`].
#[derive(Debug, Deserialize)]
struct IproxFile {
    #[serde(default)]
    accession: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

impl IproxFile {
    /// Maps a PROXI `dataFiles` entry to a [`RemoteFile`] when it is a
    /// downloadable RAW file (HTTP(S) URL); anything else maps to `None`.
    fn to_remote_file(self) -> Option<RemoteFile> {
        let is_raw = self.accession.as_deref() == Some(RAW_FILE_URI_ACCESSION)
            || self.name.as_deref() == Some("Associated raw file URI");
        let url = Iprox::normalize_url(&self.value?);
        if !is_raw || !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        let (relative_path, name) = Iprox::split_url(&url);
        if name.is_empty() {
            return None;
        }
        Some(RemoteFile {
            url,
            size: None,
            category: Some("RAW".to_string()),
            name,
            relative_path,
        })
    }
}

impl Iprox {
    /// Normalizes an iProX download URL: `download.iprox.org` presents a
    /// certificate that is only valid for `*.iprox.cn`, so the hostname is
    /// rewritten to `download.iprox.cn` (same server, valid cert) over HTTPS.
    fn normalize_url(url: &str) -> String {
        for scheme in ["https://", "http://"] {
            if let Some(rest) = url.strip_prefix(&format!("{scheme}download.iprox.org/")) {
                return format!("https://download.iprox.cn/{rest}");
            }
        }
        url.to_string()
    }

    /// Fills in `size` of every listed file that has a URL but no known size,
    /// via a HEAD request's `Content-Length`. PROXI reports no sizes, but the
    /// download server does. Best effort: a failed HEAD leaves `size` as
    /// `None`, and the download then discovers the size from the GET
    /// response's `Content-Length` as before.
    async fn fill_sizes_from_head(client: &reqwest::Client, files: &mut [RemoteFile]) {
        let sem = Arc::new(Semaphore::new(HEAD_CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for (idx, file) in files.iter().enumerate() {
            if file.size.is_some() || file.url.is_empty() {
                continue;
            }
            let permit = match sem.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => continue, // semaphore closed; skip this file
            };
            let client = client.clone();
            let url = file.url.clone();
            set.spawn(async move {
                let _permit = permit;
                let result = async {
                    let resp = client
                        .head(&url)
                        .timeout(common::API_TIMEOUT)
                        .send()
                        .await
                        .with_context(|| format!("HEAD {url} failed"))?
                        .error_for_status()
                        .with_context(|| format!("server returned an error for HEAD {url}"))?;
                    // Read the header directly: Response::content_length() is
                    // 0 for HEAD responses because they carry no body.
                    let size = resp
                        .headers()
                        .get(reqwest::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    Ok::<_, anyhow::Error>(size)
                };
                (idx, result.await)
            });
        }
        while let Some(task) = set.join_next().await {
            match task {
                Ok((idx, Ok(Some(size)))) => files[idx].size = Some(size),
                Ok((idx, Ok(None))) => {
                    debug!("HEAD {} returned no Content-Length", files[idx].url);
                }
                Ok((idx, Err(e))) => {
                    debug!("HEAD size lookup failed for {}: {e:#}", files[idx].url);
                }
                Err(e) => debug!("HEAD size lookup task panicked: {e}"),
            }
        }
    }

    /// Splits a download URL into (relative_path, name), dropping everything
    /// up to and including the `/IPX.../` project segment. E.g.
    /// `https://download.iprox.cn/IPX0019033000/IPX0019033001/a.raw` →
    /// (`IPX0019033001/a.raw`, `a.raw`).
    fn split_url(url: &str) -> (String, String) {
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/').map(|(_, p)| p))
            .unwrap_or("");
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let start = segments
            .iter()
            .position(|s| s.starts_with("IPX"))
            .map_or(0, |i| i + 1);
        let segments = &segments[start..];
        let name = segments.last().copied().unwrap_or("").to_string();
        let relative_path = segments.join("/");
        (relative_path, name)
    }
}

impl Repository for Iprox {
    async fn get_file_list(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<RemoteFile>> {
        self.get_file_list_with(client, config, accession, IPROX_PROXI_BASE)
            .await
    }

    async fn fetch_checksums(
        &self,
        _client: &reqwest::Client,
        _config: &Config,
        _accession: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        // iProX publishes no per-file checksums; downloads are size-validated.
        Ok(None)
    }
}

impl Iprox {
    /// Like [`Repository::get_file_list`], but with an explicit PROXI base
    /// URL, so tests (or mirrors) can point the listing elsewhere.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails after `config.max_retries`
    /// attempts or the response cannot be parsed.
    pub async fn get_file_list_with(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
        proxi_base: &str,
    ) -> Result<Vec<RemoteFile>> {
        let url = format!("{proxi_base}/{accession}");
        let what = format!("fetch iProX file list for {accession}");
        let record: IproxRecord = common::retry_with_backoff(&what, config.max_retries, || {
            let client = client.clone();
            let url = url.clone();
            let accession = accession.to_string();
            async move {
                client
                    .get(&url)
                    .header("Accept", "application/json")
                    .timeout(IPROX_API_TIMEOUT)
                    .send()
                    .await
                    .with_context(|| format!("failed to fetch iProX record for {accession}"))?
                    .error_for_status()
                    .with_context(|| format!("iProX returned an error for {accession}"))?
                    .json()
                    .await
                    .with_context(|| format!("failed to parse iProX record for {accession}"))
            }
        })
        .await?;

        let mut files: Vec<RemoteFile> = record
            .data_files
            .into_iter()
            .filter_map(IproxFile::to_remote_file)
            .collect();
        // PROXI lists no sizes; ask each file's URL for its Content-Length so
        // the listing, progress bars and size validation all have real sizes.
        Self::fill_sizes_from_head(client, &mut files).await;
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD_JSON: &str = r#"{
        "accession": {"accession": "MS:1001919", "value": "PXD082693"},
        "title": "some dataset",
        "dataFiles": [
            {"accession": "MS:1002846", "name": "Associated raw file URI", "value": "http://download.iprox.org/IPX0019033000/IPX0019033001/HFX2_a-1A.raw"},
            {"accession": "MS:1002846", "name": "Associated raw file URI", "value": "https://download.iprox.org/IPX0019033000/IPX0019033002/HFX2_b-1A.raw"},
            {"accession": "MS:1002849", "name": "Search engine output file URI", "value": "http://download.iprox.org/IPX0019033000/IPX0019033001/results.TXT"},
            {"accession": "MS:1002851", "name": "Other type file URI", "value": "http://download.iprox.org/IPX0019033000/IPX0019033001/metadata.xlsx"},
            {"accession": "MS:1002846", "name": "Associated raw file URI", "value": "ftp://not-http/a.raw"}
        ]
    }"#;

    #[test]
    fn iprox_api_timeout_exceeds_shared_api_timeout() {
        // The iProX PROXI endpoint routinely takes longer than 60 s to answer
        // a single dataset record, so it needs its own, larger budget.
        assert!(IPROX_API_TIMEOUT > common::API_TIMEOUT);
    }

    #[test]
    fn split_url_drops_project_segment() {
        assert_eq!(
            Iprox::split_url("https://download.iprox.cn/IPX0019033000/IPX0019033001/a.raw"),
            ("IPX0019033001/a.raw".to_string(), "a.raw".to_string())
        );
        assert_eq!(
            Iprox::split_url("https://download.iprox.cn/IPX0000001000/file.raw"),
            ("file.raw".to_string(), "file.raw".to_string())
        );
    }

    #[test]
    fn normalize_url_rewrites_iprox_org_host_to_valid_cert_host() {
        assert_eq!(
            Iprox::normalize_url("https://download.iprox.org/IPX0019033000/IPX0019033001/a.raw"),
            "https://download.iprox.cn/IPX0019033000/IPX0019033001/a.raw"
        );
        assert_eq!(
            Iprox::normalize_url("http://download.iprox.org/IPX0000001000/f.raw"),
            "https://download.iprox.cn/IPX0000001000/f.raw"
        );
        // Other hosts are left alone.
        assert_eq!(
            Iprox::normalize_url("https://example.org/x.raw"),
            "https://example.org/x.raw"
        );
    }

    #[test]
    fn record_keeps_only_raw_files_with_normalized_urls() {
        let record: IproxRecord = serde_json::from_str(RECORD_JSON).unwrap();
        let files: Vec<RemoteFile> = record
            .data_files
            .into_iter()
            .filter_map(IproxFile::to_remote_file)
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].relative_path, "IPX0019033001/HFX2_a-1A.raw");
        assert_eq!(files[0].name, "HFX2_a-1A.raw");
        // download.iprox.org is rewritten to the TLS-valid download.iprox.cn.
        assert_eq!(
            files[0].url,
            "https://download.iprox.cn/IPX0019033000/IPX0019033001/HFX2_a-1A.raw"
        );
        assert_eq!(files[0].size, None);
        assert_eq!(files[1].relative_path, "IPX0019033002/HFX2_b-1A.raw");
        assert_eq!(
            files[1].url,
            "https://download.iprox.cn/IPX0019033000/IPX0019033002/HFX2_b-1A.raw"
        );
    }
}
