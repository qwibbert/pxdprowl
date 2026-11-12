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

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::common::{self, RemoteFile};
use super::config::Config;
use super::repository::Repository;

/// iProX PROXI API base, keyed by PXD accession.
pub const IPROX_PROXI_BASE: &str = "https://www.iprox.cn/proxi/datasets";

/// cvParam accession of "Associated raw file URI".
const RAW_FILE_URI_ACCESSION: &str = "MS:1002846";

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
                    .timeout(common::API_TIMEOUT)
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

        Ok(record
            .data_files
            .into_iter()
            .filter_map(IproxFile::to_remote_file)
            .collect())
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
