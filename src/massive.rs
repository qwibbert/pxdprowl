//! [MassIVE](https://massive.ucsd.edu) implementation of [`Repository`].
//!
//! MassIVE publishes no per-file checksums, so downloads are validated by
//! size (the file index reports each file's size; the download itself is
//! checked against `Content-Length`). File lists come from the GNPS2 datasette
//! index of MassIVE files (`datasetcache.gnps2.org`); downloads go through the
//! ProteoSAFe HTTPS endpoint `DownloadResultFile`, which serves the same bytes
//! as the FTPS archive. Only the `raw/` collection is listed as RAW files.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::common::{self, RemoteFile};
use super::config::Config;
use super::repository::Repository;

/// GNPS2 datasette index of all MassIVE files (queried as `.json`).
pub const GNPS2_INDEX_BASE: &str = "https://datasetcache.gnps2.org/datasette/database.json";
/// ProteoSAFe HTTPS download endpoint.
pub const PROTEOSAFE_DOWNLOAD_BASE: &str =
    "https://massive.ucsd.edu/ProteoSAFe/DownloadResultFile";

/// The [MassIVE](https://massive.ucsd.edu) repository at UCSD.
#[derive(Debug, Clone, Copy)]
pub struct Massive;

/// One row of the GNPS2 file index for a dataset.
#[derive(Debug, Clone, Deserialize)]
struct IndexRow {
    filepath: String,
    #[serde(default)]
    size: Option<u64>,
}

/// Response envelope of a datasette `.json` query with `_shape=objects`.
#[derive(Debug, Deserialize)]
struct IndexResponse {
    rows: Vec<IndexRow>,
}

/// Percent-encoding set for the `file` query parameter: everything except
/// unreserved characters and `/` is encoded.
const QUERY_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

impl Massive {
    /// Builds the ProteoSAFe HTTPS download URL for `relpath` within dataset
    /// `msv_id` (`f.<MSV>/<relpath>` in the server's file space).
    fn download_url(download_base: &str, msv_id: &str, relpath: &str) -> String {
        let file_param = format!("f.{msv_id}/{relpath}");
        let encoded = percent_encoding::utf8_percent_encode(&file_param, QUERY_SET);
        format!("{download_base}?forceDownload=true&file={encoded}")
    }
}

impl Repository for Massive {
    async fn get_file_list(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<RemoteFile>> {
        self.get_file_list_with(client, config, accession, GNPS2_INDEX_BASE, PROTEOSAFE_DOWNLOAD_BASE)
            .await
    }

    async fn fetch_checksums(
        &self,
        _client: &reqwest::Client,
        _config: &Config,
        _accession: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        // MassIVE publishes no per-file checksums; downloads are size-validated.
        Ok(None)
    }
}

impl Massive {
    /// Like [`Repository::get_file_list`], but with explicit base URLs, so
    /// tests (or mirrors) can point the listing and download endpoints
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// Returns an error if `msv_id` is malformed, if the index request fails
    /// after `config.max_retries` attempts, or if the response cannot be
    /// parsed.
    pub async fn get_file_list_with(
        &self,
        client: &reqwest::Client,
        config: &Config,
        msv_id: &str,
        index_base: &str,
        download_base: &str,
    ) -> Result<Vec<RemoteFile>> {
        // The id is interpolated into the SQL string, so only allow plain
        // alphanumerics to keep the query well-formed.
        if msv_id.is_empty() || !msv_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("invalid MassIVE accession: {msv_id}");
        }
        let sql = format!(
            "SELECT filepath,size FROM filename WHERE dataset=\"{msv_id}\" AND filepath LIKE \"raw/%\""
        );
        let url = format!(
            "{index_base}?sql={}&_shape=objects",
            percent_encoding::utf8_percent_encode(&sql, percent_encoding::NON_ALPHANUMERIC)
        );
        let what = format!("fetch MassIVE file list for {msv_id}");
        let response: IndexResponse = common::retry_with_backoff(&what, config.max_retries, || {
            let client = client.clone();
            let url = url.clone();
            let msv_id = msv_id.to_string();
            async move {
                client
                    .get(&url)
                    .header("Accept", "application/json")
                    .timeout(common::API_TIMEOUT)
                    .send()
                    .await
                    .with_context(|| format!("failed to fetch MassIVE file index for {msv_id}"))?
                    .error_for_status()
                    .with_context(|| format!("MassIVE file index returned an error for {msv_id}"))?
                    .json()
                    .await
                    .with_context(|| format!("failed to parse MassIVE file index for {msv_id}"))
            }
        })
        .await?;

        Ok(response
            .rows
            .into_iter()
            .map(|row| {
                let name = row.filepath.rsplit('/').next().unwrap_or(&row.filepath);
                RemoteFile {
                    url: Self::download_url(download_base, msv_id, &row.filepath),
                    size: row.size,
                    category: Some("RAW".to_string()),
                    name: name.to_string(),
                    relative_path: row.filepath,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX_JSON: &str = r#"{
        "ok": true,
        "rows": [
            {"filepath": "raw/Replicate 1/a.raw", "size": 2443332650},
            {"filepath": "raw/b.raw", "size": 12},
            {"filepath": "ccms_peak/Replicate 1/a.mzML", "size": 896842736},
            {"filepath": "result/proteins.txt", "size": null}
        ],
        "columns": ["filepath", "size"]
    }"#;

    #[test]
    fn download_url_encodes_file_param_but_keeps_slashes() {
        let url = Massive::download_url("http://x/dl", "MSV000079263", "raw/Replicate 1/a b.raw");
        assert_eq!(
            url,
            "http://x/dl?forceDownload=true&file=f.MSV000079263/raw/Replicate%201/a%20b.raw"
        );
    }

    #[test]
    fn index_rows_map_to_remote_files() {
        let response: IndexResponse = serde_json::from_str(INDEX_JSON).unwrap();
        // The SQL query itself filters to raw/; the mapping keeps filepath as
        // the relative path and derives the basename.
        let rows: Vec<RemoteFile> = response
            .rows
            .iter()
            .filter(|r| r.filepath.starts_with("raw/"))
            .map(|r| {
                let name = r.filepath.rsplit('/').next().unwrap_or(&r.filepath);
                RemoteFile {
                    url: Massive::download_url("http://x/dl", "MSV000079263", &r.filepath),
                    size: r.size,
                    category: Some("RAW".to_string()),
                    name: name.to_string(),
                    relative_path: r.filepath.clone(),
                }
            })
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].relative_path, "raw/Replicate 1/a.raw");
        assert_eq!(rows[0].name, "a.raw");
        assert_eq!(rows[0].size, Some(2443332650));
        assert_eq!(
            rows[0].url,
            "http://x/dl?forceDownload=true&file=f.MSV000079263/raw/Replicate%201/a.raw"
        );
        assert_eq!(rows[1].relative_path, "raw/b.raw");
        assert_eq!(rows[1].size, Some(12));
    }
}
