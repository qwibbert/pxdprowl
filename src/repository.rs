//! The [`Repository`] trait abstracting a dataset repository.

use std::path::Path;

use anyhow::Result;
use indicatif::MultiProgress;

use super::common;
use super::config::Config;

/// A remote dataset repository that can list and download datasets.
///
/// All methods are associated functions: repositories are stateless, so a
/// unit struct implementing this trait is used purely as a namespace.
pub trait Repository {
    /// Fetches the file list for `accession` from the repository API.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails after `config.max_retries`
    /// attempts or the response cannot be parsed.
    async fn get_file_list(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<common::PrideFile>>;

    /// Downloads a single file from `url` to `dest`, resuming from a
    /// leftover `.part` file if present. Skips the download if `dest`
    /// already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be fully downloaded after
    /// `config.max_retries` attempts, or if the final rename fails.
    async fn download_file(
        client: &reqwest::Client,
        config: &Config,
        url: &str,
        dest: &Path,
        multi: &MultiProgress,
    ) -> Result<()>;

    /// Downloads all files of the dataset `accession` into
    /// `dest/{accession}/` and validates their checksums. Returns the
    /// number of files in the dataset.
    ///
    /// # Errors
    ///
    /// Returns an error if any file fails to download or any checksum
    /// validation fails.
    async fn download_dataset(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
        dest: &Path,
        multi: &MultiProgress,
    ) -> Result<usize>;
}
