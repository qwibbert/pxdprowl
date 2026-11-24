//! High-level library API: the [`Client`] facade and [`ResolvedDataset`].
//!
//! [`Client`] bundles everything a download needs — HTTP client, [`Config`],
//! shared [`ConcurrencyLimits`] and a progress display — behind a handful of
//! methods, so library users don't have to wire the lower-level modules
//! together themselves. The `cli` module (the binary) builds on the same
//! pieces. Progress bars are hidden by default; pass your own
//! [`MultiProgress`] to [`Client::with_progress`] to display them.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar};

use crate::common::{self, ConcurrencyLimits, RemoteFile};
use crate::config::{Config, DownloadRequest, dedupe_requests};
use crate::proteomexchange::{self, HostingRepository, PxDatasetInfo};
use crate::repository::{self, Repo, Repository};

/// A PXD accession resolved through ProteomeXchange to the repository that
/// hosts it, ready to list or download.
///
/// Constructed by [`ResolvedDataset::resolve`] (or [`Client::resolve`]); the
/// fields are public so the resolution result can be inspected before
/// downloading.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResolvedDataset {
    /// The PXD accession as given, e.g. `PXD000001`. Used as the output
    /// directory name and in log/error messages.
    pub accession: String,
    /// The repository hosting the dataset.
    pub repo: Repo,
    /// The repository-local id the repository's API is keyed by (the `MSV...`
    /// id for MassIVE; the PXD id itself for PRIDE and iProX).
    pub repo_id: String,
    /// The full ProteomeXchange record the resolution is based on.
    pub info: PxDatasetInfo,
}

impl ResolvedDataset {
    /// Resolves a PXD `accession` through ProteomeXchange (PROXI API) to the
    /// repository hosting it, plus the repository-local id its API is keyed
    /// by.
    ///
    /// # Errors
    ///
    /// Returns an error if `accession` is not a well-formed `PXD<digits>` id,
    /// if the ProteomeXchange lookup fails, if the dataset is hosted on a
    /// repository pxdprowl does not support, or if a MassIVE-hosted record
    /// carries no `MSV...` id.
    pub async fn resolve(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Self> {
        Self::resolve_with_base(client, config, accession, proteomexchange::PROXI_API_BASE).await
    }

    /// Like [`ResolvedDataset::resolve`], but queries `base_url` instead of
    /// [`proteomexchange::PROXI_API_BASE`].
    ///
    /// The base URL is a parameter so tests can point the resolver at a local
    /// mock server.
    ///
    /// # Errors
    ///
    /// Same as [`ResolvedDataset::resolve`].
    pub async fn resolve_with_base(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
        base_url: &str,
    ) -> Result<Self> {
        common::validate_accession(accession)?;
        let info = proteomexchange::resolve_with_base(client, config, accession, base_url).await?;
        let host = info.hosting_repository();
        let repo = match host {
            HostingRepository::Pride => Repo::Pride,
            HostingRepository::Massive => Repo::Massive,
            HostingRepository::Iprox => Repo::Iprox,
            other => bail!(
                "dataset {accession} is hosted on {} via ProteomeXchange; pxdprowl currently supports PRIDE, MassIVE and iProX",
                other.name()
            ),
        };
        let repo_id = match repo {
            Repo::Massive => info.repository_id().map(str::to_string).with_context(|| {
                format!("ProteomeXchange record for {accession} has no MassIVE id")
            })?,
            _ => accession.to_string(),
        };
        Ok(Self {
            accession: accession.to_string(),
            repo,
            repo_id,
            info,
        })
    }

    /// Fetches the RAW file list of the resolved dataset from its repository.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails after `config.max_retries`
    /// attempts or the response cannot be parsed.
    pub async fn list_files(
        &self,
        client: &reqwest::Client,
        config: &Config,
    ) -> Result<Vec<RemoteFile>> {
        self.repo
            .get_file_list(client, config, &self.repo_id)
            .await
            .with_context(|| format!("failed to fetch file list for {}", self.accession))
    }

    /// Downloads every RAW file of the resolved dataset into
    /// `config.output/{accession}/`, preserving each file's relative path,
    /// and validates them (MD5 checksums for PRIDE, size for MassIVE/iProX).
    /// Returns the number of files in the dataset.
    ///
    /// `limits` holds the batch-wide concurrency caps shared with every other
    /// dataset of the run.
    ///
    /// # Errors
    ///
    /// Returns an error if any file fails to download, the checksum fetch
    /// fails, or any file's validation fails.
    pub async fn download(
        &self,
        client: &reqwest::Client,
        config: &Config,
        limits: &Arc<ConcurrencyLimits>,
        multi: &MultiProgress,
    ) -> Result<usize> {
        let target = repository::DatasetTarget {
            repo_id: &self.repo_id,
            label: &self.accession,
            dest: &config.output,
            gate: self.repo.download_gate(),
        };
        repository::download_dataset(self.repo, client, config, limits, &target, multi)
            .await
            .with_context(|| format!("failed to download dataset {}", self.accession))
    }
}

/// High-level entry point for using pxdprowl as a library: one handle bundling
/// the HTTP client, the [`Config`], the shared [`ConcurrencyLimits`] and the
/// progress display.
///
/// # Examples
///
/// ```rust,no_run
/// # async fn example() -> anyhow::Result<()> {
/// let mut config = pxdprowl::Config::default();
/// config.output = "data".into();
///
/// let client = pxdprowl::Client::new(config)?;
///
/// // Download every RAW file of a dataset into `data/PXD000001/`.
/// let count = client.download_dataset("PXD000001").await?;
///
/// // Or just list the RAW files without downloading them.
/// let files = client.list_files("PXD000001").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
    limits: Arc<ConcurrencyLimits>,
    multi: MultiProgress,
    proxi_base: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Creates a client with `config` and a hidden progress display (no
    /// terminal output from progress bars).
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(config: Config) -> Result<Self> {
        let multi = MultiProgress::new();
        multi.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        Self::with_progress(config, multi)
    }

    /// Creates a client with `config`, drawing progress bars onto `multi`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be built.
    pub fn with_progress(config: Config, multi: MultiProgress) -> Result<Self> {
        Ok(Self {
            http: default_http_client()?,
            limits: Arc::new(ConcurrencyLimits::new(&config)),
            config,
            multi,
            proxi_base: proteomexchange::PROXI_API_BASE.to_string(),
        })
    }

    /// Replaces the HTTP client (e.g. one with custom timeouts or proxies).
    #[must_use]
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Points ProteomeXchange resolution at `base_url` instead of
    /// [`proteomexchange::PROXI_API_BASE`]. Mainly useful for tests, like
    /// [`proteomexchange::resolve_with_base`].
    #[must_use]
    pub fn with_proxi_base(mut self, base_url: impl Into<String>) -> Self {
        self.proxi_base = base_url.into();
        self
    }

    /// The settings this client runs with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolves a PXD `accession` through ProteomeXchange to the repository
    /// hosting it. See [`ResolvedDataset::resolve`].
    ///
    /// # Errors
    ///
    /// Same as [`ResolvedDataset::resolve`].
    pub async fn resolve(&self, accession: &str) -> Result<ResolvedDataset> {
        ResolvedDataset::resolve_with_base(&self.http, &self.config, accession, &self.proxi_base)
            .await
    }

    /// Lists the RAW files available for a PXD `accession`, without
    /// downloading them.
    ///
    /// # Errors
    ///
    /// Returns an error if the accession is invalid, the ProteomeXchange
    /// lookup fails, the dataset is hosted on an unsupported repository, or
    /// the file-list request fails.
    pub async fn list_files(&self, accession: &str) -> Result<Vec<RemoteFile>> {
        self.resolve(accession)
            .await?
            .list_files(&self.http, &self.config)
            .await
    }

    /// Downloads every RAW file of a PXD `accession` into
    /// `config.output/{accession}/` and returns the number of files.
    ///
    /// # Errors
    ///
    /// Returns an error if the accession is invalid, the ProteomeXchange
    /// lookup fails, the dataset is hosted on an unsupported repository, any
    /// file fails to download, or any file's validation fails.
    pub async fn download_dataset(&self, accession: &str) -> Result<usize> {
        self.resolve(accession)
            .await?
            .download(&self.http, &self.config, &self.limits, &self.multi)
            .await
    }

    /// Downloads a single file of a dataset, matched by `file_name` (basename
    /// or relative path within the dataset), into
    /// `config.output/{accession}/...`. Returns the destination path.
    ///
    /// # Errors
    ///
    /// Returns an error if the accession is invalid, the dataset cannot be
    /// resolved or listed, `file_name` is not in the dataset, the file has no
    /// downloadable URL, or the download fails.
    pub async fn download_file(&self, accession: &str, file_name: &str) -> Result<PathBuf> {
        let resolved = self.resolve(accession).await?;
        let files = resolved.list_files(&self.http, &self.config).await?;
        let file = files
            .iter()
            .find(|f| f.name == file_name || f.relative_path == file_name)
            .with_context(|| format!("file '{file_name}' not found in dataset {accession}"))?;
        if file.url.is_empty() {
            bail!("no downloadable URL for '{file_name}' in {accession}");
        }
        let dest = common::checked_join(&self.config.output.join(accession), &file.relative_path)
            .with_context(|| {
            format!("unsafe destination path for '{file_name}' in dataset {accession}")
        })?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        // Count against the global download cap, like dataset files do.
        let _permit = self
            .limits
            .downloads
            .clone()
            .acquire_owned()
            .await
            .context("download semaphore closed unexpectedly")?;
        let bars = repository::FileBars::new(self.multi.clone(), None);
        repository::download_file(&self.http, &self.config, &file.url, &dest, &bars, file.size)
            .await?;
        Ok(dest)
    }

    /// Downloads a file directly from `url` into `config.output/`. The local
    /// file name is derived from the URL's last path segment. Returns the
    /// destination path.
    ///
    /// Only HTTPS URLs are accepted, plus plain HTTP for loopback hosts; see
    /// [`common::download_url_file_name`].
    ///
    /// # Errors
    ///
    /// Returns an error if `url` is not an allowed HTTP(S) URL or the download
    /// fails.
    pub async fn download_url(&self, url: &str) -> Result<PathBuf> {
        let file_name = common::download_url_file_name(url)?;
        tokio::fs::create_dir_all(&self.config.output)
            .await
            .with_context(|| format!("failed to create {}", self.config.output.display()))?;
        let dest = self.config.output.join(&file_name);
        // Count against the global download cap, like dataset files do.
        let _permit = self
            .limits
            .downloads
            .clone()
            .acquire_owned()
            .await
            .context("download semaphore closed unexpectedly")?;
        let bars = repository::FileBars::new(self.multi.clone(), None);
        repository::download_file(&self.http, &self.config, url, &dest, &bars, None).await?;
        Ok(dest)
    }

    /// Runs a single [`DownloadRequest`] (dataset, file within a dataset, or
    /// URL).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails; see [`Client::download_dataset`],
    /// [`Client::download_file`] and [`Client::download_url`].
    pub async fn download_request(&self, request: &DownloadRequest) -> Result<()> {
        match request {
            DownloadRequest::Dataset(accession) => {
                self.download_dataset(accession).await?;
            }
            DownloadRequest::File {
                accession,
                file_name,
            } => {
                self.download_file(accession, file_name).await?;
            }
            DownloadRequest::Url(url) => {
                self.download_url(url).await?;
            }
        }
        Ok(())
    }

    /// Runs a batch of [`DownloadRequest`]s concurrently (so a small dataset
    /// doesn't delay the others), all sharing this client's concurrency
    /// limits. Duplicates are skipped; failures are collected and summarized
    /// at the end instead of stopping at the first one.
    ///
    /// # Errors
    ///
    /// Returns an error naming every failed request if any of them failed.
    pub async fn download(&self, requests: &[DownloadRequest]) -> Result<()> {
        let downloads = dedupe_requests(requests);
        // With multiple requests, show an overall count bar on top of the
        // per-request bars (hidden unless the client was built with a
        // visible `MultiProgress`).
        let overall = (downloads.len() > 1).then(|| {
            let bar = self.multi.add(ProgressBar::new(downloads.len() as u64));
            bar.set_style(common::count_style());
            bar.set_prefix("Downloads");
            bar
        });
        let mut set = tokio::task::JoinSet::new();
        for request in &downloads {
            let this = self.clone();
            let overall = overall.clone();
            let request = request.clone();
            let label = request.label();
            set.spawn(async move {
                log::info!("starting {label}");
                let result = this.download_request(&request).await;
                if let Some(bar) = &overall {
                    bar.inc(1);
                }
                (label, result)
            });
        }
        // Collect results as tasks complete (completion order, not spawn
        // order) and summarize failures at the end.
        let mut failed = Vec::new();
        while let Some(task_result) = set.join_next().await {
            match task_result {
                Ok((_label, Ok(()))) => {}
                Ok((label, Err(e))) => {
                    log::warn!("{label} failed: {e:#}");
                    failed.push(label);
                }
                Err(e) => {
                    log::warn!("download task panicked: {e}");
                    failed.push("unknown".to_string());
                }
            }
        }
        if let Some(bar) = &overall {
            bar.finish_and_clear();
        }
        if failed.is_empty() {
            Ok(())
        } else {
            bail!("{} download(s) failed: {}", failed.len(), failed.join(", "))
        }
    }
}

/// The HTTP client [`Client`] builds by default: a connect timeout of
/// [`common::DOWNLOAD_CONNECT_TIMEOUT`] and reqwest's other defaults.
fn default_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(common::DOWNLOAD_CONNECT_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_builds_a_client_with_the_given_config() {
        let config = Config {
            output: PathBuf::from("data"),
            download_concurrency: 3,
            ..Config::default()
        };
        let client = Client::new(config.clone()).unwrap();
        assert_eq!(client.config(), &config);
    }

    #[test]
    fn with_proxi_base_overrides_the_resolution_endpoint() {
        let client = Client::new(Config::default())
            .unwrap()
            .with_proxi_base("http://127.0.0.1:9/proxi");
        assert_eq!(client.proxi_base, "http://127.0.0.1:9/proxi");
    }

    #[tokio::test]
    async fn resolve_rejects_malformed_accession_before_any_request() {
        let client = Client::new(Config::default()).unwrap();
        let err = client.resolve("not an accession").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid accession"),
            "err: {err:#}"
        );
    }

    #[tokio::test]
    async fn download_dataset_rejects_malformed_accession_before_any_request() {
        let client = Client::new(Config::default()).unwrap();
        let err = client
            .download_dataset("PXD000001?foo=bar")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid accession"),
            "err: {err:#}"
        );
    }

    #[tokio::test]
    async fn download_url_rejects_non_http_scheme_before_any_request() {
        let client = Client::new(Config::default()).unwrap();
        let err = client.download_url("file:///etc/passwd").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("unsupported URL scheme"),
            "err: {err:#}"
        );
    }
}
