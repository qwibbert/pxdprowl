//! Command-line interface for `pxdprowl`.
//!
//! Defines the clap argument structure and the [`run`] dispatcher that
//! executes each subcommand against the PRIDE archive.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar};
use log::{debug, info, warn};

use crate::client::ResolvedDataset;
use crate::common;
use crate::config::{
    CONFIG_FILE_NAME, CliOverrides, Config, DownloadRequest, ResolvedConfig, dedupe_requests,
};
use crate::repository::{self, Repo, Repository};
use crate::update;

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

    /// Disable the automatic update check against crates.io.
    /// Overrides PXDPROWL_UPDATE_CHECK and the `update-check` key in
    /// pxdprowl.toml. The `update` subcommand checks regardless of this flag.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    /// A dataset accession given without a subcommand, e.g. `pxdprowl
    /// PXD000001`. Instead of failing with an "unrecognized subcommand"
    /// error, pxdprowl prints a hint with the commands available for that
    /// dataset.
    #[arg(value_name = "DATASET")]
    pub accession: Option<String>,

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

    /// Check crates.io for a newer pxdprowl release.
    Update,
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
        /// URL of the file to download: HTTPS, or HTTP for loopback hosts only.
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
        no_update_check: args.no_update_check,
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

    if args.command.is_some() && args.accession.is_some() {
        bail!(
            "unexpected argument {:?}: a dataset accession cannot be combined with a subcommand",
            args.accession.as_deref().unwrap_or_default()
        );
    }

    if args.command.is_some() {
        hint_subcommand_overrides_config(&resolved);
    }

    let result = match &args.command {
        // Listing is not a download; run it directly.
        Some(Commands::Files { accession }) => cmd_files(&client, config, accession, &multi).await,
        // An explicit update check runs directly too (and is not followed by
        // the automatic one below).
        Some(Commands::Update) => cmd_update(&client).await,
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
                Commands::Update => unreachable!("Update is dispatched above"),
            };
            cmd_download(&client, config, &limits, &action, &multi).await
        }
        None => {
            // A bare dataset accession (`pxdprowl PXD000001`): show the hint
            // instead of the config-file behavior / "unrecognized subcommand".
            if let Some(arg) = &args.accession {
                return cmd_bare_accession(arg, &resolved);
            }
            if resolved.downloads.is_empty() {
                // Nothing to do: show the help plus a hint instead of erroring.
                use std::io::{IsTerminal as _, Write as _};
                let colours = common::colours_enabled(std::io::stdout().is_terminal());
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(help_with_hint(&resolved, colours)?.as_bytes())
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
                let request = request.clone();
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
    };

    // Automatic update check, rate-limited to once per
    // update::UPDATE_CHECK_INTERVAL and skipped for `pxdprowl update` (which
    // checks explicitly). It runs after the command's own output and is
    // best-effort: it never fails the command.
    if config.update_check && !matches!(args.command, Some(Commands::Update)) {
        notify_if_update_available(&client).await;
    }
    result
}

/// Renders the long help plus a hint about subcommands / the config file,
/// shown when `pxdprowl` runs without a subcommand and with nothing to
/// download. The `Hint:` label is coloured (cargo-style bold yellow) when
/// `colours` is true.
fn help_with_hint(resolved: &ResolvedConfig, colours: bool) -> Result<String> {
    use std::fmt::Write as _;
    let hint_label = common::hint_style("Hint:", colours);
    let mut cmd = Args::command();
    let mut buf = Vec::new();
    cmd.write_long_help(&mut buf)
        .context("failed to render help")?;
    let mut text = String::from_utf8(buf).context("help output is not valid UTF-8")?;
    match &resolved.config_path {
        Some(path) => {
            write!(
                text,
                "\n{hint_label} {} was loaded but lists no downloads. Add `datasets`, `files` or \
                 `urls` to it, or pass a subcommand.\n",
                path.display()
            )
            .context("failed to render hint")?;
        }
        None => {
            write!(
                text,
                "\n{hint_label} pass a subcommand, or create a {CONFIG_FILE_NAME} with \
                 `datasets`/`files`/`urls` and run `pxdprowl` without arguments.\n"
            )
            .context("failed to render hint")?;
        }
    }
    Ok(text)
}

/// Handles `pxdprowl PXD000001` — a bare dataset accession with no
/// subcommand. A well-formed PXD accession gets a hint listing the commands
/// that can be run on that dataset; anything else keeps the old clap-style
/// "unrecognized subcommand" error.
fn cmd_bare_accession(arg: &str, resolved: &ResolvedConfig) -> Result<()> {
    use std::io::IsTerminal as _;
    if common::validate_accession(arg).is_err() {
        let colours = common::colours_enabled(std::io::stderr().is_terminal());
        let mut cmd = Args::command();
        let err = cmd.error(
            clap::error::ErrorKind::InvalidSubcommand,
            format!("unrecognized subcommand {arg:?}"),
        );
        // Strip clap's own "error: " prefix; main() adds it back via anyhow.
        // Keep the ANSI styling when the error will land on a coloured terminal.
        let rendered = if colours {
            err.render().ansi().to_string()
        } else {
            err.render().to_string()
        };
        let msg = rendered.trim_start_matches("error: ").trim_end();
        bail!("{msg}");
    }
    use std::io::Write as _;
    let colours = common::colours_enabled(std::io::stdout().is_terminal());
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(accession_hint(arg, resolved, colours).as_bytes())
        .context("failed to print hint")?;
    Ok(())
}

/// Renders the hint shown for `pxdprowl PXD000001` (a bare dataset accession,
/// no subcommand): the commands that can be run on that dataset, plus a
/// pointer to the config-file batch mode. The `Hint:` label is coloured
/// (cargo-style bold yellow) and the accession is bold when `colours` is true.
fn accession_hint(accession: &str, resolved: &ResolvedConfig, colours: bool) -> String {
    use std::fmt::Write as _;
    let hint_label = common::hint_style("Hint:", colours);
    let accession_bold = common::bold_style(&format!("`{accession}`"), colours);
    let mut text = String::new();
    // write! into a String is infallible; the ignores keep clippy quiet.
    let _ = writeln!(
        text,
        "{hint_label} {accession_bold} is a dataset accession, but no subcommand was given. Try one of:"
    );
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "  pxdprowl dataset {accession}          # download all RAW files of the dataset"
    );
    let _ = writeln!(
        text,
        "  pxdprowl files {accession}            # list the RAW files of the dataset"
    );
    let _ = writeln!(
        text,
        "  pxdprowl file name {accession} FILE   # download one file by name"
    );
    let _ = writeln!(
        text,
        "  pxdprowl file url URL                 # download a file directly from a URL"
    );
    let _ = writeln!(
        text,
        "  pxdprowl update                       # check for a newer pxdprowl release"
    );
    let _ = writeln!(text);
    match &resolved.config_path {
        Some(path) => {
            let _ = writeln!(
                text,
                "Or edit {} to list downloads and run `pxdprowl` with no arguments.",
                path.display()
            );
        }
        None => {
            let _ = writeln!(
                text,
                "Or create a {CONFIG_FILE_NAME} with `datasets`/`files`/`urls` and run `pxdprowl` with no arguments."
            );
        }
    }
    text
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
    common::validate_accession(accession)?;
    let (repo, repo_id) = fetch_with_spinner(
        multi,
        &format!("Resolving {accession} via ProteomeXchange..."),
        resolve_repo(client, config, accession),
    )
    .await?;
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
async fn cmd_files(
    client: &reqwest::Client,
    config: &Config,
    accession: &str,
    multi: &MultiProgress,
) -> Result<()> {
    common::validate_accession(accession)?;
    let (repo, repo_id) = fetch_with_spinner(
        multi,
        &format!("Resolving {accession} via ProteomeXchange..."),
        resolve_repo(client, config, accession),
    )
    .await?;
    let files = fetch_with_spinner(
        multi,
        &format!("Fetching file list for {accession}..."),
        repo.get_file_list(client, config, &repo_id),
    )
    .await
    .with_context(|| format!("failed to fetch file list for {accession}"))?;

    if files.is_empty() {
        println!("No RAW files found for {accession}.");
        return Ok(());
    }

    let name_width = files
        .iter()
        .map(|f| f.relative_path.len())
        .max()
        .unwrap_or(0);
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
    println!("{}", format_total_size(&files));
    Ok(())
}

/// Runs an explicit update check (`pxdprowl update`) and prints the result.
/// Unlike the automatic check, a failed check is an error here: the user
/// asked for it.
async fn cmd_update(client: &reqwest::Client) -> Result<()> {
    use std::io::IsTerminal as _;
    let colours = common::colours_enabled(std::io::stdout().is_terminal());
    match update::check(client, true).await {
        Ok(update::UpdateStatus::Available(latest)) => {
            println!("{}", update::update_notice(&latest, colours));
        }
        Ok(update::UpdateStatus::UpToDate) => {
            println!("pxdprowl {} is up to date.", update::current_version());
        }
        Err(e) => bail!("update check failed: {e:#}"),
    }
    Ok(())
}

/// Runs the rate-limited automatic update check and prints a notice to
/// stderr when a newer release is available. Never fails: check errors are
/// logged at debug level only.
async fn notify_if_update_available(client: &reqwest::Client) {
    use std::io::IsTerminal as _;
    let colours = common::colours_enabled(std::io::stderr().is_terminal());
    match update::check(client, false).await {
        Ok(update::UpdateStatus::Available(latest)) => {
            eprintln!("{}", update::update_notice(&latest, colours));
        }
        Ok(update::UpdateStatus::UpToDate) => {}
        Err(e) => debug!("update check failed: {e:#}"),
    }
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
    common::validate_accession(accession)?;
    let (repo, repo_id) = fetch_with_spinner(
        multi,
        &format!("Resolving {accession} via ProteomeXchange..."),
        resolve_repo(client, config, accession),
    )
    .await?;
    let files = fetch_with_spinner(
        multi,
        &format!("Fetching file list for {accession}..."),
        repo.get_file_list(client, config, &repo_id),
    )
    .await
    .with_context(|| format!("failed to fetch file list for {accession}"))?;

    let file = files
        .iter()
        .find(|f| f.name == file_name || f.relative_path == file_name)
        .with_context(|| format!("file '{file_name}' not found in dataset {accession}"))?;

    if file.url.is_empty() {
        bail!("no downloadable URL for '{file_name}' in {accession}");
    }

    let dest = common::checked_join(&config.output.join(accession), &file.relative_path)
        .with_context(|| {
            format!("unsafe destination path for '{file_name}' in dataset {accession}")
        })?;
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
    let file_name = common::download_url_file_name(url)?;

    tokio::fs::create_dir_all(&config.output)
        .await
        .with_context(|| format!("failed to create {}", config.output.display()))?;
    let dest = config.output.join(&file_name);

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

/// Runs `fut` while showing a ticking spinner with `msg`, so a slow API call
/// (the iProX PROXI endpoint can take over a minute per record) doesn't look
/// like a hang. The spinner is cleared on success and on error alike.
async fn fetch_with_spinner<T>(
    multi: &MultiProgress,
    msg: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let bar = multi.add(ProgressBar::new_spinner());
    bar.set_style(common::spinner_style());
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    let result = fut.await;
    bar.finish_and_clear();
    result
}

/// Resolves a PXD `accession` through ProteomeXchange to the repository
/// hosting it, plus the repository-local id its API is keyed by (the `MSV...`
/// id for MassIVE; the PXD id itself for PRIDE and iProX).
///
/// Thin wrapper over [`ResolvedDataset::resolve`], which holds the actual
/// mapping logic and is shared with library users.
///
/// # Errors
///
/// Returns an error if the ProteomeXchange lookup fails, if the dataset is
/// hosted on a repository pxdprowl does not support, or if a MassIVE-hosted
/// record carries no `MSV...` id.
async fn resolve_repo(
    client: &reqwest::Client,
    config: &Config,
    accession: &str,
) -> Result<(Repo, String)> {
    let resolved = ResolvedDataset::resolve(client, config, accession).await?;
    Ok((resolved.repo, resolved.repo_id))
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

/// Sums the known file sizes in `files`, returning the total in bytes plus
/// the number of files whose size the repository did not report.
fn total_size(files: &[common::RemoteFile]) -> (u64, usize) {
    let mut total = 0u64;
    let mut unknown = 0usize;
    for file in files {
        match file.size {
            Some(size) => total = total.saturating_add(size),
            None => unknown += 1,
        }
    }
    (total, unknown)
}

/// Renders the total-size line of the `files` listing: the sum of the known
/// sizes, noting how many files had no reported size (those are not included
/// in the sum, or the total is unknown when none of them do).
fn format_total_size(files: &[common::RemoteFile]) -> String {
    let (total, unknown) = total_size(files);
    if unknown == 0 {
        format!("Total size: {}", format_size(total))
    } else if unknown == files.len() {
        format!("Total size: unknown ({unknown} file(s) without a reported size)")
    } else {
        format!(
            "Total size: {} ({unknown} file(s) without a reported size not included)",
            format_size(total)
        )
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

    fn remote_file(name: &str, size: Option<u64>) -> common::RemoteFile {
        common::RemoteFile {
            name: name.to_string(),
            relative_path: name.to_string(),
            url: format!("https://example.org/{name}"),
            size,
            category: Some("RAW".to_string()),
        }
    }

    #[test]
    fn total_size_sums_all_known_sizes() {
        let files = vec![
            remote_file("a.raw", Some(100)),
            remote_file("b.raw", Some(2048)),
        ];
        assert_eq!(total_size(&files), (2148, 0));
    }

    #[test]
    fn total_size_counts_files_with_unknown_size() {
        let files = vec![
            remote_file("a.raw", Some(100)),
            remote_file("b.raw", None),
            remote_file("c.raw", None),
        ];
        assert_eq!(total_size(&files), (100, 2));
    }

    #[test]
    fn total_size_all_unknown_counts_every_file() {
        let files = vec![remote_file("a.raw", None), remote_file("b.raw", None)];
        assert_eq!(total_size(&files), (0, 2));
    }

    #[test]
    fn total_size_empty_list_is_zero() {
        assert_eq!(total_size(&[]), (0, 0));
    }

    #[test]
    fn total_size_saturates_instead_of_overflowing() {
        let files = vec![
            remote_file("a.raw", Some(u64::MAX)),
            remote_file("b.raw", Some(1)),
        ];
        assert_eq!(total_size(&files), (u64::MAX, 0));
    }

    #[test]
    fn format_total_size_all_sizes_known() {
        let files = vec![
            remote_file("a.raw", Some(100)),
            remote_file("b.raw", Some(2048)),
        ];
        assert_eq!(format_total_size(&files), "Total size: 2.1 KiB");
    }

    #[test]
    fn format_total_size_some_sizes_unknown_notes_the_omission() {
        let files = vec![remote_file("a.raw", Some(100)), remote_file("b.raw", None)];
        assert_eq!(
            format_total_size(&files),
            "Total size: 100 B (1 file(s) without a reported size not included)"
        );
    }

    #[test]
    fn format_total_size_all_sizes_unknown_is_unknown() {
        let files = vec![remote_file("a.raw", None), remote_file("b.raw", None)];
        assert_eq!(
            format_total_size(&files),
            "Total size: unknown (2 file(s) without a reported size)"
        );
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

    #[test]
    fn args_parse_no_update_check_flag() {
        let args =
            Args::try_parse_from(["pxdprowl", "--no-update-check", "files", "PXD000001"]).unwrap();
        assert!(args.no_update_check);
        assert!(matches!(args.command, Some(Commands::Files { .. })));
        // Off by default.
        let args = Args::try_parse_from(["pxdprowl", "files", "PXD000001"]).unwrap();
        assert!(!args.no_update_check);
    }

    #[test]
    fn args_parse_update_subcommand() {
        let args = Args::try_parse_from(["pxdprowl", "update"]).unwrap();
        assert!(matches!(args.command, Some(Commands::Update)));
    }

    #[test]
    fn args_parse_bare_accession_as_positional() {
        let args = Args::try_parse_from(["pxdprowl", "PXD000001"]).unwrap();
        assert_eq!(args.accession.as_deref(), Some("PXD000001"));
        assert!(args.command.is_none());
        // Global flags still work in front of the positional.
        let args = Args::try_parse_from(["pxdprowl", "--no-update-check", "PXD000001"]).unwrap();
        assert_eq!(args.accession.as_deref(), Some("PXD000001"));
        assert!(args.no_update_check);
        assert!(args.command.is_none());
    }

    #[test]
    fn args_parse_subcommand_does_not_fill_positional() {
        let args = Args::try_parse_from(["pxdprowl", "files", "PXD000001"]).unwrap();
        assert!(args.accession.is_none());
        assert!(matches!(args.command, Some(Commands::Files { .. })));
        let args = Args::try_parse_from(["pxdprowl", "dataset", "PXD000001"]).unwrap();
        assert!(args.accession.is_none());
        assert!(matches!(args.command, Some(Commands::Dataset { .. })));
    }

    #[test]
    fn accession_hint_lists_the_dataset_commands() {
        let hint = accession_hint("PXD000001", &resolved_without_config(), false);
        assert!(
            hint.contains("Hint: `PXD000001` is a dataset accession"),
            "hint was:\n{hint}"
        );
        assert!(
            hint.contains("pxdprowl dataset PXD000001"),
            "hint was:\n{hint}"
        );
        assert!(
            hint.contains("pxdprowl files PXD000001"),
            "hint was:\n{hint}"
        );
        assert!(
            hint.contains("pxdprowl file name PXD000001 FILE"),
            "hint was:\n{hint}"
        );
        assert!(hint.contains("pxdprowl file url URL"), "hint was:\n{hint}");
        assert!(hint.contains("pxdprowl update"), "hint was:\n{hint}");
        // Without a config file, the hint points at creating one.
        assert!(hint.contains("create a pxdprowl.toml"), "hint was:\n{hint}");
        // Plain when colours are off.
        assert!(!hint.contains('\x1b'), "hint was:\n{hint}");
    }

    #[test]
    fn accession_hint_is_coloured_when_colours_on() {
        let hint = accession_hint("PXD000001", &resolved_without_config(), true);
        assert!(
            hint.contains(
                "\x1b[1;33mHint:\x1b[0m \x1b[1m`PXD000001`\x1b[0m is a dataset accession"
            ),
            "hint was:\n{hint}"
        );
        // The command lines keep the plain accession so the columns stay aligned.
        assert!(
            hint.contains("  pxdprowl dataset PXD000001          #"),
            "hint was:\n{hint}"
        );
    }

    #[test]
    fn accession_hint_names_the_loaded_config_file() {
        let resolved = ResolvedConfig {
            config_path: Some(PathBuf::from("my/pxdprowl.toml")),
            ..resolved_without_config()
        };
        let hint = accession_hint("PXD000001", &resolved, false);
        assert!(hint.contains("my/pxdprowl.toml"), "hint was:\n{hint}");
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
        let text = help_with_hint(&resolved_without_config(), false).unwrap();
        assert!(text.contains("Usage:"));
        assert!(text.contains("Hint: pass a subcommand, or create a pxdprowl.toml"));
        assert!(!text.contains('\x1b'), "text was:\n{text}");
    }

    #[test]
    fn help_hint_with_config_names_the_loaded_file() {
        let resolved = ResolvedConfig {
            config_path: Some(PathBuf::from("my/pxdprowl.toml")),
            ..resolved_without_config()
        };
        let text = help_with_hint(&resolved, false).unwrap();
        assert!(text.contains("my/pxdprowl.toml was loaded but lists no downloads"));
    }

    #[test]
    fn help_hint_colours_the_hint_label_when_colours_on() {
        let text = help_with_hint(&resolved_without_config(), true).unwrap();
        assert!(
            text.contains("\x1b[1;33mHint:\x1b[0m pass a subcommand"),
            "text was:\n{text}"
        );
    }

    #[tokio::test]
    async fn cmd_dataset_rejects_malformed_accession_before_any_request() {
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let err = cmd_dataset(&client, &config, &limits, "PXD000001?foo=bar", &multi)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid accession"));
    }

    #[tokio::test]
    async fn cmd_files_rejects_malformed_accession_before_any_request() {
        let config = Config::default();
        let client = reqwest::Client::new();
        let multi = MultiProgress::new();
        let err = cmd_files(&client, &config, "../etc", &multi)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid accession"));
    }

    #[tokio::test]
    async fn cmd_file_by_name_rejects_malformed_accession_before_any_request() {
        let config = Config::default();
        let client = reqwest::Client::new();
        let limits = Arc::new(common::ConcurrencyLimits::new(&config));
        let multi = MultiProgress::new();
        let err = cmd_file_by_name(
            &client,
            &config,
            &limits,
            "not an accession",
            "a.raw",
            &multi,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("invalid accession"));
    }
}
