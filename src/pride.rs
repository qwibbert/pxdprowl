//! [PRIDE](https://www.ebi.ac.uk/pride) archive implementation of [`Repository`].
//!
//! PRIDE is the only supported repository that publishes checksums (an MD5
//! TSV per dataset), so its downloads are MD5-validated; the shared download
//! engine in [`crate::repository`] handles the rest.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::common::{self, PrideFile, RemoteFile};
use super::config::Config;
use super::repository::Repository;
use super::validate;

/// The [PRIDE](https://www.ebi.ac.uk/pride) proteomics repository at EBI.
#[derive(Debug, Clone, Copy)]
pub struct Pride;

/// Maps a PRIDE API file record onto the repository-neutral [`RemoteFile`].
/// PRIDE datasets have a flat layout, so the relative path is the file name.
fn to_remote_file(file: PrideFile) -> RemoteFile {
    let name = file.file_name.clone();
    RemoteFile {
        url: file.https_url(),
        size: file.file_size,
        category: file.file_category.map(|c| c.value),
        name: name.clone(),
        relative_path: name,
    }
}

impl Repository for Pride {
    async fn get_file_list(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<RemoteFile>> {
        let url = format!("{}/projects/{accession}/files/all", common::PRIDE_API_BASE);
        let what = format!("fetch file list for {accession}");
        let files: Vec<common::PrideFile> =
            common::retry_with_backoff(&what, config.max_retries, || {
                let client = client.clone();
                let url = url.clone();
                async move {
                    client
                        .get(&url)
                        .header("Accept", "application/json")
                        .timeout(common::API_TIMEOUT)
                        .send()
                        .await
                        .with_context(|| format!("failed to fetch file list for {accession}"))?
                        .json()
                        .await
                        .with_context(|| format!("failed to parse file list JSON for {accession}"))
                }
            })
            .await?;

        Ok(files
            .into_iter()
            .filter(|f| f.file_category.as_ref().is_some_and(|c| c.value == "RAW"))
            .map(to_remote_file)
            .collect())
    }

    async fn fetch_checksums(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        Ok(Some(validate::fetch_checksums(client, config, accession).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::common::{Category, FileLocation};

    #[test]
    fn pride_file_maps_to_remote_file_with_flat_path() {
        let file = PrideFile {
            file_name: "sample.raw".to_string(),
            file_category: Some(Category {
                value: "RAW".to_string(),
            }),
            locations: vec![FileLocation {
                name: "FTP Protocol".to_string(),
                value: format!("{}pride/data/archive/2024/sample.raw", common::PRIDE_FTP_PREFIX),
            }],
            file_size: Some(42),
        };
        let remote = to_remote_file(file);
        assert_eq!(remote.name, "sample.raw");
        assert_eq!(remote.relative_path, "sample.raw");
        assert_eq!(
            remote.url,
            format!("{}pride/data/archive/2024/sample.raw", common::PRIDE_HTTPS_PREFIX)
        );
        assert_eq!(remote.size, Some(42));
        assert_eq!(remote.category.as_deref(), Some("RAW"));
    }
}
