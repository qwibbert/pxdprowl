//! Command-line interface for `pride-rs`.
//!
//! Defines the clap argument structure and the [`run`] dispatcher that
//! executes each subcommand against the PRIDE archive.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::MultiProgress;
use log::info;

use crate::common;
use crate::{Pride, Repository};

// ---------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------

/// Download datasets and files from the PRIDE proteomics archive.
#[derive(Parser, Debug)]
#[command(name = "pride-rs", version, about, long_about = None)]
pub struct Args {
    /// Directory to store downloaded files.
    #[arg(short, long, default_value = "datasets", global = true)]
    pub output: PathBuf,

    /// Increase logging verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

/// The available subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Download all RAW files of a PRIDE dataset.
    Dataset {
        /// PRIDE accession, e.g. `PXD000001`.
        accession: String,
    },

    /// List the RAW files available for a PRIDE dataset.
    Files {
        /// PRIDE accession, e.g. `PXD000001`.
        accession: String,
    },

    /// Download a single file.
    File {
        #[command(subcommand)]
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

/// Parses CLI args (done by the caller) and runs the requested subcommand.
///
/// Sets up logging, the HTTP client, and the progress-bar display, then
/// dispatches to the appropriate command handler.
///
/// # Errors
///
/// Returns an error if the logger cannot be installed, the HTTP client
/// cannot be built, or the subcommand fails.
pub async fn run(args: Args) -> Result<()> {
    let multi = MultiProgress::new();
    init_logging(&multi, args.verbose)?;

    let client = reqwest::Client::builder()
        .connect_timeout(common::DOWNLOAD_CONNECT_TIMEOUT)
        .build()
        .context("failed to build HTTP client")?;

    info!(
        "Concurrency: download={}, validate={} ({} CPUs)",
        *common::DOWNLOAD_CONCURRENCY,
        common::VALIDATE_CONCURRENCY.map_or_else(|| "aimd".to_string(), |n| n.to_string()),
        common::cpu_count()
    );

    match &args.command {
        Commands::Dataset { accession } => {
            cmd_dataset(&client, accession, &args.output, &multi).await
        }
        Commands::Files { accession } => cmd_files(&client, accession).await,
        Commands::File { source } => match source {
            FileSource::Name {
                accession,
                file_name,
            } => cmd_file_by_name(&client, accession, file_name, &args.output, &multi).await,
            FileSource::Url { url } => cmd_file_by_url(&client, url, &args.output, &multi).await,
        },
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Downloads every RAW file of `accession` into `output/{accession}/` and
/// validates checksums.
async fn cmd_dataset(
    client: &reqwest::Client,
    accession: &str,
    output: &Path,
    multi: &MultiProgress,
) -> Result<()> {
    let count = Pride::download_dataset(client, accession, output, multi)
        .await
        .with_context(|| format!("failed to download dataset {accession}"))?;
    println!(
        "Downloaded {count} file(s) for {accession} into {}",
        output.join(accession).display()
    );
    Ok(())
}

/// Prints the RAW file list for `accession`.
async fn cmd_files(client: &reqwest::Client, accession: &str) -> Result<()> {
    let files = Pride::get_file_list(client, accession)
        .await
        .with_context(|| format!("failed to fetch file list for {accession}"))?;

    if files.is_empty() {
        println!("No RAW files found for {accession}.");
        return Ok(());
    }

    let name_width = files.iter().map(|f| f.file_name.len()).max().unwrap_or(0);
    for file in &files {
        let size = file.file_size.map_or_else(|| "?".to_string(), format_size);
        println!(
            "{:<name_width$}  {:>12}",
            file.file_name,
            size,
            name_width = name_width
        );
    }
    println!("\n{} RAW file(s) found for {accession}.", files.len());
    Ok(())
}

/// Downloads a single file identified by `accession` + `file_name`.
async fn cmd_file_by_name(
    client: &reqwest::Client,
    accession: &str,
    file_name: &str,
    output: &Path,
    multi: &MultiProgress,
) -> Result<()> {
    let files = Pride::get_file_list(client, accession)
        .await
        .with_context(|| format!("failed to fetch file list for {accession}"))?;

    let file = files
        .iter()
        .find(|f| f.file_name == file_name)
        .with_context(|| format!("file '{file_name}' not found in dataset {accession}"))?;

    let url = file.https_url();
    if url.is_empty() {
        bail!("no downloadable HTTPS URL for '{file_name}' in {accession}");
    }

    let dest_dir = output.join(accession);
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;
    let dest = dest_dir.join(file_name);

    Pride::download_file(client, &url, &dest, multi).await?;
    println!("Downloaded {file_name} -> {}", dest.display());
    Ok(())
}

/// Downloads a single file directly from `url`.
async fn cmd_file_by_url(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    multi: &MultiProgress,
) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
    let file_name = parsed
        .path_segments()
        .and_then(|mut segs| segs.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download");

    tokio::fs::create_dir_all(output)
        .await
        .with_context(|| format!("failed to create {}", output.display()))?;
    let dest = output.join(file_name);

    Pride::download_file(client, url, &dest, multi).await?;
    println!("Downloaded {file_name} -> {}", dest.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
}
