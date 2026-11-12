//! The [`Repository`] trait abstracting a dataset repository, the [`Repo`]
//! dispatcher picked by ProteomeXchange resolution, and the shared download
//! engine every repository implementation uses.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar};
use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use super::common;
use super::config::Config;
use super::validate::{validate_file, validate_size};
use super::{Iprox, Massive, Pride};

/// A remote dataset repository that can list and download datasets.
///
/// Repositories are stateless unit structs, so the trait requires [`Copy`] and
/// a value is used purely as a namespace for the `&self` methods. The methods
/// return `Send` futures (declared as RPITIT so generic code like the shared
/// engine can spawn them); implementations may use plain `async fn`.
pub trait Repository: Copy + Send + Sync + 'static {
    /// Fetches the file list for `accession` (the repository-local id, e.g.
    /// the `MSV...` id for MassIVE) from the repository API.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails after `config.max_retries`
    /// attempts or the response cannot be parsed.
    fn get_file_list(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> impl std::future::Future<Output = Result<Vec<common::RemoteFile>>> + Send;

    /// Fetches the checksum map (file name -> expected MD5 hex digest) for
    /// `accession`, or `Ok(None)` when the repository publishes no checksums
    /// (downloads are then validated by size instead).
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails after `config.max_retries`
    /// attempts or the response cannot be read.
    fn fetch_checksums(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> impl std::future::Future<Output = Result<Option<HashMap<String, String>>>> + Send;
}

/// A repository pxdprowl can download from, picked by ProteomeXchange
/// resolution of the PXD accession.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Repo {
    /// [PRIDE](https://www.ebi.ac.uk/pride) at EMBL-EBI.
    Pride,
    /// [MassIVE](https://massive.ucsd.edu) at UCSD.
    Massive,
    /// [iProX](https://www.iprox.org), Beijing.
    Iprox,
}

impl Repo {
    /// An optional extra gate limiting concurrent downloads from this
    /// repository's endpoint. MassIVE's ProteoSAFe download servlet answers
    /// HTTP 429 even under a pair of concurrent downloads, so its downloads
    /// are fully serialized through a process-wide single-slot gate.
    pub fn download_gate(&self) -> Option<Arc<Semaphore>> {
        match self {
            Self::Massive => Some(massive_gate()),
            _ => None,
        }
    }
}

/// Process-wide gate for MassIVE downloads (the rate-limited servlet). One
/// slot: even a pair of concurrent downloads trips MassIVE's mod_qos 429.
static MASSIVE_GATE: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();

fn massive_gate() -> Arc<Semaphore> {
    MASSIVE_GATE
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
}

/// Marker error for HTTP 429 responses. Rate limiting is transient, so
/// [`download_file`] gives these their own retry budget, separate from
/// `max_retries`.
#[derive(Debug)]
struct RateLimited;

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("rate limited by server (HTTP 429)")
    }
}

impl std::error::Error for RateLimited {}

/// Wait after a 429 when the server sends no `Retry-After` header (MassIVE's
/// mod_qos sends none; its ban is short-lived, and sequential requests spaced
/// >= 2s apart are accepted).
const RATE_LIMIT_DEFAULT_WAIT_SECS: u64 = 10;
/// Extra rounds of `max_retries` attempts for downloads that keep getting 429.
const RATE_LIMIT_ROUNDS: u32 = 3;
/// Pause between rate-limit rounds.
const RATE_LIMIT_ROUND_PAUSE: std::time::Duration = std::time::Duration::from_secs(5);

impl Repository for Repo {
    async fn get_file_list(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<common::RemoteFile>> {
        match self {
            Self::Pride => Pride.get_file_list(client, config, accession).await,
            Self::Massive => Massive.get_file_list(client, config, accession).await,
            Self::Iprox => Iprox.get_file_list(client, config, accession).await,
        }
    }

    async fn fetch_checksums(
        &self,
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Option<HashMap<String, String>>> {
        match self {
            Self::Pride => Pride.fetch_checksums(client, config, accession).await,
            Self::Massive => Massive.fetch_checksums(client, config, accession).await,
            Self::Iprox => Iprox.fetch_checksums(client, config, accession).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared download engine
// ---------------------------------------------------------------------------

/// Downloads a single file from `url` to `dest`, resuming from a leftover
/// `.part` file if present. Skips the download if `dest` already exists.
///
/// `expected_size` is the size reported by the repository's file listing, used
/// for the progress bar and the completeness check when the server sends no
/// `Content-Length` (chunked responses, e.g. MassIVE's download servlet).
///
/// # Errors
///
/// Returns an error if the file cannot be fully downloaded after
/// `config.max_retries` attempts, or if the final rename fails.
pub async fn download_file(
    client: &reqwest::Client,
    config: &Config,
    url: &str,
    dest: &Path,
    multi: &MultiProgress,
    expected_size: Option<u64>,
) -> Result<()> {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if dest.exists() {
        info!("Skipping {file_name} (already exists)");
        return Ok(());
    }

    let tmp = dest.with_extension("part");
    let what = file_name.clone();
    // 429 responses are transient and get their own budget: each round runs
    // the normal `max_retries` loop, and up to RATE_LIMIT_ROUNDS extra rounds
    // run (with a pause between) as long as the server keeps rate-limiting —
    // a rate-limited file is retried until it goes through, not failed.
    let mut round = 0u32;
    let result = loop {
        let attempt = common::retry_with_backoff(&what, config.max_retries, || {
            let client = client.clone();
            let url = url.to_string();
            let dest = dest.to_path_buf();
            let tmp = tmp.clone();
            let file_name = file_name.clone();
            let multi = multi.clone();
            async move {
                try_download_once(&client, &url, &dest, &tmp, &file_name, &multi, expected_size)
                    .await
            }
        })
        .await;
        match attempt {
            Err(e) if e.downcast_ref::<RateLimited>().is_some() && round < RATE_LIMIT_ROUNDS => {
                round += 1;
                warn!(
                    "{file_name}: still rate limited after a full retry round; pausing {RATE_LIMIT_ROUND_PAUSE:?} and trying again (round {round}/{RATE_LIMIT_ROUNDS})"
                );
                tokio::time::sleep(RATE_LIMIT_ROUND_PAUSE).await;
            }
            other => break other,
        }
    };
    result?;
    Ok(())
}

/// Identifies the dataset a download run targets.
#[derive(Debug, Clone)]
pub struct DatasetTarget<'a> {
    /// Repository-local accession the repository's own API is keyed by (the
    /// PXD id for PRIDE/iProX, the `MSV...` id for MassIVE).
    pub repo_id: &'a str,
    /// User-facing accession (the PXD id), used for the output directory and
    /// log lines.
    pub label: &'a str,
    /// Parent output directory; files land in `dest/{label}/`.
    pub dest: &'a Path,
    /// Optional extra gate limiting how many of this dataset's files download
    /// concurrently (used for endpoints that rate-limit, e.g. MassIVE's
    /// servlet). `None` = only the global download cap applies.
    pub gate: Option<Arc<Semaphore>>,
}

/// Shared checksum cell: `Ok(Some(map))` when the repository publishes MD5
/// checksums, `Ok(None)` when it publishes none, `Err` when the fetch failed.
type ChecksumCell = Arc<tokio::sync::OnceCell<Result<Option<HashMap<String, String>>>>>;

/// Downloads every listed file of the dataset `target` into
/// `dest/{label}/`, preserving each file's relative path, and validates each
/// file inline as soon as its own download completes — by MD5 checksum when
/// the repository publishes checksums, otherwise by size when the listing
/// reports one. Returns the number of files in the dataset.
///
/// `limits` holds the batch-wide concurrency caps shared with every other
/// dataset of the run, so `download_concurrency` and `validate_concurrency`
/// remain global limits when several datasets are downloaded concurrently.
///
/// # Errors
///
/// Returns an error if any file fails to download, the checksum fetch fails,
/// or any file's validation fails.
pub async fn download_dataset<R: Repository>(
    repo: R,
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    target: &DatasetTarget<'_>,
    multi: &MultiProgress,
) -> Result<usize> {
    let DatasetTarget {
        repo_id,
        label,
        dest,
        gate,
    } = target;
    let files = repo.get_file_list(client, config, repo_id).await?;
    info!("{label}: {} file(s) listed", files.len());

    let output_dir = dest.join(label);
    tokio::fs::create_dir_all(&output_dir).await?;

    // Per-dataset file-count bar.
    let dl_bar = multi.add(ProgressBar::new(files.len() as u64));
    dl_bar.set_style(common::count_style());
    dl_bar.set_prefix(format!("{label} download"));

    // Fetch the checksums concurrently with the downloads: each file is
    // validated inline as soon as its own download completes, so the
    // CPU-bound MD5 hashing overlaps the remaining (network-bound) downloads
    // instead of running as a separate phase afterwards.
    let checksums: ChecksumCell = Arc::new(tokio::sync::OnceCell::new());
    let checksums_fetch = tokio::spawn({
        let client = client.clone();
        let config = config.clone();
        let repo_id = repo_id.to_string();
        let checksums = checksums.clone();
        async move {
            checksums
                .get_or_init(|| repo.fetch_checksums(&client, &config, &repo_id))
                .await;
        }
    });
    limits.start_aimd();

    // Download missing files with limited concurrency. The semaphore is
    // shared across all datasets of the run, so `download_concurrency` stays
    // a global cap. JoinSet processes results as they complete (not spawn
    // order), so a slow resume doesn't block already-finished skips.
    let sem = limits.downloads.clone();
    let mut set = tokio::task::JoinSet::new();
    for file in &files {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .context("semaphore closed unexpectedly")?;
        // Repositories with a rate-limited endpoint (MassIVE) additionally
        // pass through a small gate, so their servlet is not hammered.
        let gate_permit = match gate {
            Some(g) => Some(
                g.clone()
                    .acquire_owned()
                    .await
                    .context("download gate closed unexpectedly")?,
            ),
            None => None,
        };
        let client = client.clone();
        let config = config.clone();
        let limits = limits.clone();
        let repo = repo;
        let file = file.clone();
        let repo_id = repo_id.to_string();
        let label = label.to_string();
        let dest = output_dir.join(&file.relative_path);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let multi = multi.clone();
        let dl_bar = dl_bar.clone();
        let checksums = checksums.clone();
        let output_dir = output_dir.clone();
        set.spawn(async move {
            // Hold the download permit (and gate, if any) only for the
            // download itself: inline validation must not occupy a slot.
            let result = {
                let _permit = permit;
                let _gate_permit = gate_permit;
                download_file(&client, &config, &file.url, &dest, &multi, file.size).await
            };
            dl_bar.inc(1);
            // Validate inline, overlapping the remaining downloads.
            let validation = match &result {
                Ok(()) => {
                    InlineValidation {
                        repo,
                        checksums: &checksums,
                        client: &client,
                        config: &config,
                        limits: &limits,
                        repo_id: &repo_id,
                        label: &label,
                        output_dir: &output_dir,
                        multi: &multi,
                    }
                    .validate(&file)
                    .await
                }
                Err(_) => Ok(()),
            };
            (file.relative_path.clone(), result, validation)
        });
    }

    let mut failed_downloads = Vec::new();
    let mut failed_validations = Vec::new();
    while let Some(task_result) = set.join_next().await {
        match task_result {
            Ok((rel_path, download, validation)) => {
                if let Err(e) = download {
                    warn!("{label}/{rel_path}: download failed: {e:#}");
                    failed_downloads.push(rel_path.clone());
                }
                // validate_file / validate_size already logged the per-file specifics.
                if validation.is_err() {
                    failed_validations.push(rel_path);
                }
            }
            Err(e) => {
                warn!("{label}: task panicked: {e}");
                failed_downloads.push("unknown".to_string());
            }
        }
    }
    dl_bar.finish_and_clear();

    // Surface a failed checksum fetch only now (after the downloads), matching
    // the previous validate-after-download ordering.
    checksums_fetch
        .await
        .context("checksum fetch task panicked")?;
    if let Some(Err(e)) = checksums.get() {
        bail!("failed to fetch checksums for {label}: {e:#}");
    }

    if !failed_downloads.is_empty() {
        bail!(
            "{} file(s) failed to download: {}",
            failed_downloads.len(),
            failed_downloads.join(", ")
        );
    }
    if !failed_validations.is_empty() {
        bail!(
            "validation failed for {} file(s): {}",
            failed_validations.len(),
            failed_validations.join(", ")
        );
    }
    info!("{label}: all files validated");

    Ok(files.len())
}

/// Shared context for validating one downloaded file inline: cheap references
/// to the per-dataset state that every file task already holds.
struct InlineValidation<'a, R: Repository> {
    repo: R,
    checksums: &'a ChecksumCell,
    client: &'a reqwest::Client,
    config: &'a Config,
    limits: &'a Arc<common::ConcurrencyLimits>,
    repo_id: &'a str,
    label: &'a str,
    output_dir: &'a Path,
    multi: &'a MultiProgress,
}

impl<R: Repository> InlineValidation<'_, R> {
    /// Validates one downloaded file: by MD5 against the checksum map when the
    /// repository publishes one, otherwise by size when the listing reports
    /// one. A missing checksum entry or a failed checksum fetch skips
    /// validation here; the fetch error is surfaced by the caller after the
    /// download phase.
    async fn validate(&self, file: &common::RemoteFile) -> Result<()> {
        let cell = self
            .checksums
            .get_or_init(|| self.repo.fetch_checksums(self.client, self.config, self.repo_id))
            .await;
        let path = self.output_dir.join(&file.relative_path);
        match cell {
            Ok(Some(map)) => {
                if map.is_empty() {
                    return Ok(());
                }
                let Some(expected) = map.get(&file.name) else {
                    warn!(
                        "{}/{}: no checksum found, skipping",
                        self.label, file.relative_path
                    );
                    return Ok(());
                };
                let size = file
                    .size
                    .unwrap_or_else(|| std::fs::metadata(&path).map_or(0, |m| m.len()));
                validate_file(
                    self.limits,
                    self.label,
                    self.output_dir,
                    &file.relative_path,
                    expected,
                    size,
                    self.multi,
                )
                .await
            }
            // No checksums published by this repository: fall back to size.
            Ok(None) => match file.size {
                Some(expected) => validate_size(self.label, &path, expected).await,
                None => {
                    info!(
                        "{}/{}: no checksum or size published, skipping validation",
                        self.label, file.relative_path
                    );
                    Ok(())
                }
            },
            Err(_) => Ok(()),
        }
    }
}

/// Single download attempt. Re-reads the `.part` file size so retries resume
/// from where the previous attempt left off.
async fn try_download_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    tmp: &Path,
    file_name: &str,
    multi: &MultiProgress,
    expected_size: Option<u64>,
) -> Result<()> {
    // Resume support: if a .part file exists from a previous interrupted
    // download, ask the server for the remaining bytes via Range header.
    let resume_from = tokio::fs::metadata(tmp).await.map_or(0, |m| m.len());

    let mut req = client.get(url);
    if resume_from > 0 {
        info!("Resuming {file_name} from {resume_from} bytes");
        req = req.header("Range", format!("bytes={resume_from}-"));
    } else {
        info!("Downloading {file_name}");
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("failed to GET {url}"))?;

    // 416 Range Not Satisfiable: the .part file is already the complete file.
    if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && resume_from > 0 {
        info!("{file_name}: .part file already complete, finalizing");
        tokio::fs::rename(tmp, dest).await?;
        return Ok(());
    }

    // 429 Too Many Requests: wait out the server's Retry-After (MassIVE's
    // mod_qos sends none, so default to a longer pause) so retries don't
    // hammer a rate-limited endpoint. The marker error gets its own retry
    // budget in download_file, outside the normal max_retries count.
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(RATE_LIMIT_DEFAULT_WAIT_SECS);
        info!("{file_name}: rate limited (429), waiting {retry_after}s before retry");
        tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
        return Err(anyhow::Error::new(RateLimited));
    }

    let mut resp = resp
        .error_for_status()
        .with_context(|| format!("server returned error for {url}"))?;

    let is_partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    // Chunked responses carry no Content-Length; fall back to the size the
    // repository's listing reported. On a 206 resume, that's the remaining
    // bytes, not the file total.
    let total = resp.content_length().unwrap_or_else(|| match expected_size {
        Some(exp) if is_partial => exp.saturating_sub(resume_from),
        Some(exp) => exp,
        None => 0,
    });
    // When resuming with a 206 response, content_length is the *remaining*
    // bytes — add the already-downloaded portion to get the true total.
    let already = if is_partial {
        resume_from
    } else {
        // Server ignored Range (200 OK) — restart from scratch.
        if resume_from > 0 {
            warn!("{file_name}: server does not support Range, restarting from scratch");
        }
        0
    };
    let grand_total = total + already;

    // Per-file byte bar with ETA, starting at the resumed offset.
    let pb = multi.add(ProgressBar::new(grand_total));
    pb.set_style(common::bytes_style());
    pb.set_prefix(file_name.to_string());
    pb.set_position(already);

    // Open in append mode if resuming, truncate otherwise.
    let mut file = if already > 0 {
        tokio::fs::OpenOptions::new().append(true).open(tmp).await?
    } else {
        tokio::fs::File::create(tmp).await?
    };
    loop {
        // Per-chunk read timeout: if no data arrives for DOWNLOAD_READ_TIMEOUT,
        // treat as a stalled connection and retry (resuming from .part).
        let chunk = tokio::time::timeout(common::DOWNLOAD_READ_TIMEOUT, resp.chunk())
            .await
            .context(format!(
                "{file_name}: read timed out after {:?}",
                common::DOWNLOAD_READ_TIMEOUT
            ))?
            .context("failed to read response chunk")?;
        match chunk {
            Some(bytes) => {
                file.write_all(&bytes).await?;
                pb.inc(bytes.len() as u64);
            }
            None => break,
        }
    }
    file.flush().await?;
    drop(file);

    // Sanity check: if the server told us a total and we got less, the
    // download was incomplete.
    if grand_total > 0 && pb.position() < grand_total {
        anyhow::bail!(
            "{file_name}: incomplete download ({}/{} bytes), .part file kept for resume",
            pb.position(),
            grand_total
        );
    }

    tokio::fs::rename(tmp, dest).await?;
    pb.finish_and_clear();

    info!("Downloaded {file_name} ({grand_total} bytes)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // md5("hello")
    const MD5_HELLO: &str = "5d41402abc4b2a76b9719d911017c592";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pxdprowl-repo-test-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A repository whose listing and checksums are canned, so the engine runs
    /// without network access.
    #[derive(Clone, Copy)]
    struct StubRepo {
        files: &'static [common::RemoteFile],
        checksums: Option<&'static [(&'static str, &'static str)]>,
    }

    impl Repository for StubRepo {
        async fn get_file_list(
            &self,
            _client: &reqwest::Client,
            _config: &Config,
            _accession: &str,
        ) -> Result<Vec<common::RemoteFile>> {
            Ok(self.files.to_vec())
        }

        async fn fetch_checksums(
            &self,
            _client: &reqwest::Client,
            _config: &Config,
            _accession: &str,
        ) -> Result<Option<HashMap<String, String>>> {
            Ok(self.checksums.map(|entries| {
                entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }))
        }
    }

    fn remote_file(rel: &'static str, size: Option<u64>) -> common::RemoteFile {
        let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
        common::RemoteFile {
            name,
            relative_path: rel.to_string(),
            url: String::new(),
            size,
            category: Some("RAW".to_string()),
        }
    }

    /// Builds an InlineValidation whose checksum cell is pre-set, so no
    /// network fetch happens.
    fn validator<'a, R: Repository>(
        repo: R,
        checksums: &'a ChecksumCell,
        client: &'a reqwest::Client,
        config: &'a Config,
        limits: &'a Arc<common::ConcurrencyLimits>,
        output_dir: &'a Path,
        multi: &'a MultiProgress,
    ) -> InlineValidation<'a, R> {
        InlineValidation {
            repo,
            checksums,
            client,
            config,
            limits,
            repo_id: "PXD000001",
            label: "PXD000001",
            output_dir,
            multi,
        }
    }

    fn checksum_cell(value: Result<Option<HashMap<String, String>>>) -> ChecksumCell {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        cell.set(value).unwrap();
        cell
    }

    fn md5_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn inline_validation_ok_on_matching_checksum() {
        let dir = temp_dir("ok");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Ok(Some(md5_map(&[("a.raw", MD5_HELLO)]))));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("a.raw", Some(5)))
            .await
            .unwrap();
        assert!(dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn inline_validation_mismatch_errors_and_deletes_file() {
        let dir = temp_dir("mismatch");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Ok(Some(md5_map(&[("a.raw", &"0".repeat(32))]))));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        let err = validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("a.raw", Some(5)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(!dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn inline_validation_skips_file_missing_from_checksum_map() {
        let dir = temp_dir("unknown");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Ok(Some(md5_map(&[("other.raw", MD5_HELLO)]))));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("a.raw", Some(5)))
            .await
            .unwrap();
        assert!(dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn inline_validation_skips_when_checksum_fetch_failed() {
        let dir = temp_dir("fetch-failed");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Err(anyhow::anyhow!("boom")));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        // The fetch error is surfaced by the caller, not here: validation skips.
        validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("a.raw", Some(5)))
            .await
            .unwrap();
        assert!(dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn inline_validation_falls_back_to_size_without_checksums() {
        let dir = temp_dir("size-ok");
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        std::fs::write(dir.join("raw").join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Ok(None));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("raw/a.raw", Some(5)))
            .await
            .unwrap();
        assert!(dir.join("raw").join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn inline_validation_size_mismatch_errors_and_deletes_file() {
        let dir = temp_dir("size-mismatch");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let checksums = checksum_cell(Ok(None));
        let repo = StubRepo {
            files: &[],
            checksums: None,
        };
        let err = validator(repo, &checksums, &client, &config, &limits, &dir, &multi)
            .validate(&remote_file("a.raw", Some(999)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("size mismatch"));
        assert!(!dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
