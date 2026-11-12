//! Command-line interface for `pxdprowl`.
//!
//! Defines the clap argument structure and the [`run`] dispatcher that
//! executes each subcommand against the PRIDE archive.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar};
use log::{info, warn};

use crate::common;
use crate::config::{CONFIG_FILE_NAME, CliOverrides, Config, DownloadRequest, ResolvedConfig};
use crate::proteomexchange::{self, HostingRepository};
use crate::repository::{self, Repo, Repository};

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

/// Download datasets and files from the PRIDE proteomics archive.
///
/// With no subcommand, the downloads listed in the config file are run.
#[derive(Parser, Debug)]
#[command(name = "pxdprowl", version, about, long_about = None)]
pub struct Args {
    /// Path to the config file. Defaults to ./pxdprowl.toml when present.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Directory to store downloaded files. Overrides PXDPROWL_OUTPUT and the
    /// `output` key in the config file. Defaults to `datasets`.
    #[arg(short, long, global = true, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// Increase logging verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Maximum number of files downloaded in parallel.
    /// Overrides PXDPROWL_DOWNLOAD_CONCURRENCY and pxdprowl.toml.
    #[arg(long, global = true, value_name = "N")]
    pub download_concurrency: Option<usize>,

    /// Fixed number of parallel checksum validations (disables the adaptive
    /// limiter). Overrides PXDPROWL_VALIDATE_CONCURRENCY and pxdprowl.toml.
    #[arg(long, global = true, value_name = "N")]
    pub validate_concurrency: Option<usize>,

    /// Maximum number of attempts for retryable operations.
    /// Overrides PXDPROWL_MAX_RETRIES and pxdprowl.toml.
    #[arg(long, global = true, value_name = "N")]
    pub max_retries: Option<usize>,

    /// The subcommand to execute.
    /// When not specified, check if a config file is present.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// The available subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Download all RAW files of a ProteomeXchange dataset.
    Dataset {
        /// ProteomeXchange accession, e.g. `PXD000001`.
        accession: String,
    },

    /// List the RAW files available for a ProteomeXchange dataset.
    Files {
        /// ProteomeXchange accession, e.g. `PXD000001`.
        accession: String,
    },

    /// Download a single file from a ProteomeXchange dataset.
    File {
        #[command(subcommand)]
        /// ProteomeXchange dataset accession + filename
        /// or file URL
        source: FileSource,
    },
}

/// Where to fetch a single file from.
#[derive(Subcommand, Debug)]
pub enum FileSource {
    /// Download a file by its name within a dataset.
    Name {
        /// PRIDE accession, e.g. `PXD000001`.
        accession: String,

        /// File name within the dataset, e.g. `sample.raw`.
        file_name: String,
    },

    /// Download a file directly from a URL.
    Url {
        /// HTTPS URL of the file to download.
        url: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parses CLI args (done by the caller) and runs the requested subcommand —
/// or, when no subcommand is given, the downloads listed in the config file.
/// With neither, prints the help plus a hint instead of failing.
///
/// Sets up logging, resolves the configuration, builds the HTTP client and
/// the progress-bar display, then dispatches to the command handlers.
///
/// # Errors
///
/// Returns an error if the logger cannot be installed, the configuration is
/// invalid, the HTTP client cannot be built, or a download fails.
pub async fn run(args: Args) -> Result<()> {
    let multi = MultiProgress::new();
    init_logging(&multi, args.verbose)?;

    // Resolve settings and download plan: CLI flags > PXDPROWL_* env vars >
    // config file (--config, else ./pxdprowl.toml) > built-in defaults.
    let resolved = Config::resolve(&CliOverrides {
        config: args.config.clone(),
        output: args.output.clone(),
        download_concurrency: args.download_concurrency,
        validate_concurrency: args.validate_concurrency,
        max_retries: args.max_retries,
    })
    .context("invalid configuration")?;
    let config = &resolved.config;

    let client = reqwest::Client::builder()
        .connect_timeout(common::DOWNLOAD_CONNECT_TIMEOUT)
        .build()
        .context("failed to build HTTP client")?;

    // One set of concurrency limits for the whole run, shared by every
    // download request so the configured caps stay global when multiple
    // datasets run concurrently.
    let limits = Arc::new(common::ConcurrencyLimits::new(config));

    info!(
        "Config: output={}, download-concurrency={}, validate-concurrency={}, max-retries={} ({} CPUs)",
        config.output.display(),
        config.download_concurrency,
        config
            .validate_concurrency
            .map_or_else(|| "aimd".to_string(), |n| n.to_string()),
        config.max_retries,
        common::cpu_count()
    );

    if args.command.is_some() {
        hint_subcommand_overrides_config(&resolved);
    }

    match &args.command {
        // Listing is not a download; run it directly.
        Some(Commands::Files { accession }) => cmd_files(&client, config, accession).await,
        Some(command) => {
            let action = match command {
                Commands::Dataset { accession } => DownloadRequest::Dataset(accession.clone()),
                Commands::File {
                    source:
                        FileSource::Name {
                            accession,
                            file_name,
                        },
                } => DownloadRequest::File {
                    accession: accession.clone(),
                    file_name: file_name.clone(),
                },
                Commands::File {
                    source: FileSource::Url { url },
                } => DownloadRequest::Url(url.clone()),
                Commands::Files { .. } => unreachable!("Files is dispatched above"),
            };
            cmd_download(&client, config, &limits, &action, &multi).await
        }
        None => {
            if resolved.downloads.is_empty() {
                // Nothing to do: show the help plus a hint instead of erroring.
                use std::io::Write as _;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(help_with_hint(&resolved)?.as_bytes())
                    .context("failed to print help")?;
                return Ok(());
            }
            // Batch from the config file: run every request concurrently so a
            // small dataset doesn't delay the others, collect failures, and
            // summarize at the end instead of stopping at the first one.
            let downloads = dedupe_requests(&resolved.downloads);
            // With multiple requests, show an overall count bar on top of the
            // per-request bars.
            let overall = (downloads.len() > 1).then(|| {
                let bar = multi.add(ProgressBar::new(downloads.len() as u64));
                bar.set_style(common::count_style());
                bar.set_prefix("Downloads");
                bar
            });
            let mut set = tokio::task::JoinSet::new();
            for request in &downloads {
                let client = client.clone();
                let config = config.clone();
                let limits = limits.clone();
                let multi = multi.clone();
                let overall = overall.clone();
                let request = (*request).clone();
                let label = request.label();
                set.spawn(async move {
                    info!("starting {label}");
                    let result = cmd_download(&client, &config, &limits, &request, &multi).await;
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
                        warn!("{label} failed: {e:#}");
                        failed.push(label);
                    }
                    Err(e) => {
                        warn!("download task panicked: {e}");
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
}

/// Renders the long help plus a hint about subcommands / the config file,
/// shown when `pxdprowl` runs without a subcommand and with nothing to download.
fn help_with_hint(resolved: &ResolvedConfig) -> Result<String> {
    use std::fmt::Write as _;
    let mut cmd = Args::command();
    let mut buf = Vec::new();
    cmd.write_long_help(&mut buf)
        .context("failed to render help")?;
    let mut text = String::from_utf8(buf).context("help output is not valid UTF-8")?;
    match &resolved.config_path {
        Some(path) => {
            write!(
                text,
                "\nHint: {} was loaded but lists no downloads. Add `datasets`, `files` or \
                 `urls` to it, or pass a subcommand.\n",
                path.display()
            )
            .context("failed to render hint")?;
        }
        None => {
            write!(
                text,
                "\nHint: pass a subcommand, or create a {CONFIG_FILE_NAME} with \
                 `datasets`/`files`/`urls` and run `pxdprowl` without arguments.\n"
            )
            .context("failed to render hint")?;
        }
    }
    Ok(text)
}

/// When a subcommand is given, the config file's download lists don't apply.
/// Show a hint instead of silently ignoring them (the file's settings still
/// apply).
fn hint_subcommand_overrides_config(resolved: &ResolvedConfig) {
    let Some(path) = &resolved.config_path else {
        return;
    };
    if resolved.downloads.is_empty() {
        warn!("using settings from {}", path.display());
    } else {
        warn!(
            "a subcommand was given, so the {} download(s) listed in {} are ignored (its settings still apply)",
            resolved.downloads.len(),
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Runs a single download action (from a subcommand or the config file).
async fn cmd_download(
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    request: &DownloadRequest,
    multi: &MultiProgress,
) -> Result<()> {
    match request {
        DownloadRequest::Dataset(accession) => {
            cmd_dataset(client, config, limits, accession, multi).await
        }
        DownloadRequest::File {
            accession,
            file_name,
        } => cmd_file_by_name(client, config, limits, accession, file_name, multi).await,
        DownloadRequest::Url(url) => cmd_file_by_url(client, config, limits, url, multi).await,
    }
}

/// Downloads every RAW file of `accession` into `output/{accession}/`,
/// preserving each file's relative path, and validates them (MD5 checksums
/// for PRIDE, size for MassIVE/iProX).
async fn cmd_dataset(
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    accession: &str,
    multi: &MultiProgress,
) -> Result<()> {
    let (repo, repo_id) = resolve_repo(client, config, accession).await?;
    let target = repository::DatasetTarget {
        repo_id: &repo_id,
        label: accession,
        dest: &config.output,
        gate: repo.download_gate(),
    };
    let count = repository::download_dataset(repo, client, config, limits, &target, multi)
        .await
        .with_context(|| format!("failed to download dataset {accession}"))?;
    println!(
        "Downloaded {count} file(s) for {accession} into {}",
        config.output.join(accession).display()
    );
    Ok(())
}

/// Prints the RAW file list for `accession`.
async fn cmd_files(client: &reqwest::Client, config: &Config, accession: &str) -> Result<()> {
    let (repo, repo_id) = resolve_repo(client, config, accession).await?;
    let files = repo
        .get_file_list(client, config, &repo_id)
        .await
        .with_context(|| format!("failed to fetch file list for {accession}"))?;

    if files.is_empty() {
        println!("No RAW files found for {accession}.");
        return Ok(());
    }

    let name_width = files.iter().map(|f| f.relative_path.len()).max().unwrap_or(0);
    for file in &files {
        let size = file.size.map_or_else(|| "?".to_string(), format_size);
        println!(
            "{:<name_width$}  {:>12}",
            file.relative_path,
            size,
            name_width = name_width
        );
    }
    println!("\n{} RAW file(s) found for {accession}.", files.len());
    Ok(())
}

/// Downloads a single file identified by `accession` + `file_name` (matched
/// by basename or relative path within the dataset).
async fn cmd_file_by_name(
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    accession: &str,
    file_name: &str,
    multi: &MultiProgress,
) -> Result<()> {
    let (repo, repo_id) = resolve_repo(client, config, accession).await?;
    let files = repo
        .get_file_list(client, config, &repo_id)
        .await
        .with_context(|| format!("failed to fetch file list for {accession}"))?;

    let file = files
        .iter()
        .find(|f| f.name == file_name || f.relative_path == file_name)
        .with_context(|| format!("file '{file_name}' not found in dataset {accession}"))?;

    if file.url.is_empty() {
        bail!("no downloadable URL for '{file_name}' in {accession}");
    }

    let dest = config.output.join(accession).join(&file.relative_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Count against the global download cap, like dataset files do.
    let _permit = limits
        .downloads
        .clone()
        .acquire_owned()
        .await
        .context("download semaphore closed unexpectedly")?;
    let bars = repository::FileBars::new(multi.clone(), None);
    repository::download_file(client, config, &file.url, &dest, &bars, file.size).await?;
    println!("Downloaded {file_name} -> {}", dest.display());
    Ok(())
}

/// Downloads a single file directly from `url`.
async fn cmd_file_by_url(
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    url: &str,
    multi: &MultiProgress,
) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    let file_name = parsed
        .path_segments()
        .and_then(|mut segs| segs.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download");

    tokio::fs::create_dir_all(&config.output)
        .await
        .with_context(|| format!("failed to create {}", config.output.display()))?;
    let dest = config.output.join(file_name);

    // Count against the global download cap, like dataset files do.
    let _permit = limits
        .downloads
        .clone()
        .acquire_owned()
        .await
        .context("download semaphore closed unexpectedly")?;
    let bars = repository::FileBars::new(multi.clone(), None);
    repository::download_file(client, config, url, &dest, &bars, None).await?;
    println!("Downloaded {file_name} -> {}", dest.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolves a PXD `accession` through ProteomeXchange to the repository
/// hosting it, plus the repository-local id its API is keyed by (the `MSV...`
/// id for MassIVE; the PXD id itself for PRIDE and iProX).
///
/// # Errors
///
/// Returns an error if the ProteomeXchange lookup fails, if the dataset is
/// hosted on a repository pxdprowl does not support, or if a MassIVE-hosted
/// record carries no `MSV...` id.
async fn resolve_repo(client: &reqwest::Client, config: &Config, accession: &str) -> Result<(Repo, String)> {
    let info = proteomexchange::resolve(client, config, accession).await?;
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
        Repo::Massive => info
            .repository_id()
            .map(str::to_string)
            .with_context(|| format!("ProteomeXchange record for {accession} has no MassIVE id"))?,
        _ => accession.to_string(),
    };
    Ok((repo, repo_id))
}

/// Deduplicates the batch's download requests, keeping the first occurrence
/// of each. With concurrent execution, two tasks fetching the same file would
/// race on the same `.part` file, so duplicates must not run twice.
fn dedupe_requests(requests: &[DownloadRequest]) -> Vec<&DownloadRequest> {
    let mut seen = HashSet::new();
    requests
        .iter()
        .filter(|request| {
            if seen.insert((*request).clone()) {
                true
            } else {
                warn!("duplicate download request skipped: {}", request.label());
                false
            }
        })
        .collect()
}

/// Installs `env_logger` bridged through `indicatif-log-bridge` so log lines
/// don't garble the progress bars. Verbosity is controlled by the `-v` count
/// or `RUST_LOG`.
fn init_logging(multi: &MultiProgress, verbose: u8) -> Result<()> {
    let default_filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
            .build();
    let level = logger.filter();
    indicatif_log_bridge::LogWrapper::new(multi.clone(), logger)
        .try_init()
        .context("failed to install global logger")?;
    log::set_max_level(level);
    Ok(())
}

/// Formats a byte count as a human-readable string, e.g. `1.2 GB`.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
    }

    #[test]
    fn format_size_kib() {
        assert_eq!(format_size(2048), "2.0 KiB");
    }

    #[test]
    fn format_size_gib() {
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn args_help_parses() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn args_parse_without_subcommand() {
        let args = Args::try_parse_from(["pxdprowl"]).unwrap();
        assert!(args.command.is_none());
        assert!(args.config.is_none());
        assert!(args.output.is_none());
    }

    #[test]
    fn args_parse_config_and_output_flags() {
        let args = Args::try_parse_from([
            "pxdprowl",
            "-c",
            "my.toml",
            "-o",
            "out",
            "dataset",
            "PXD000001",
        ])
        .unwrap();
        assert_eq!(args.config, Some(PathBuf::from("my.toml")));
        assert_eq!(args.output, Some(PathBuf::from("out")));
        assert!(matches!(args.command, Some(Commands::Dataset { .. })));
    }

    fn resolved_without_config() -> ResolvedConfig {
        ResolvedConfig {
            config: Config::default(),
            downloads: vec![],
            config_path: None,
        }
    }

    #[test]
    fn help_hint_without_config_suggests_creating_one() {
        let text = help_with_hint(&resolved_without_config()).unwrap();
        assert!(text.contains("Usage:"));
        assert!(text.contains("Hint: pass a subcommand, or create a pxdprowl.toml"));
    }

    #[test]
    fn help_hint_with_config_names_the_loaded_file() {
        let resolved = ResolvedConfig {
            config_path: Some(PathBuf::from("my/pxdprowl.toml")),
            ..resolved_without_config()
        };
        let text = help_with_hint(&resolved).unwrap();
        assert!(text.contains("my/pxdprowl.toml was loaded but lists no downloads"));
    }

    #[test]
    fn dedupe_requests_keeps_first_occurrence_of_each() {
        let requests = vec![
            DownloadRequest::Dataset("PXD000001".to_string()),
            DownloadRequest::Url("https://x/y".to_string()),
            DownloadRequest::Dataset("PXD000001".to_string()),
            DownloadRequest::File {
                accession: "PXD000001".to_string(),
                file_name: "a.raw".to_string(),
            },
            DownloadRequest::Url("https://x/y".to_string()),
        ];
        let deduped = dedupe_requests(&requests);
        assert_eq!(deduped.len(), 3);
        assert_eq!(deduped[0].label(), "dataset PXD000001");
        assert_eq!(deduped[1].label(), "url https://x/y");
        assert_eq!(deduped[2].label(), "file PXD000001/a.raw");
    }

    #[test]
    fn dedupe_requests_empty_batch_stays_empty() {
        assert!(dedupe_requests(&[]).is_empty());
    }
}
