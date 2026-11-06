//! [PRIDE](https://www.ebi.ac.uk/pride) archive implementation of [`Repository`].

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar};
use log::{info, warn};
use tokio::io::AsyncWriteExt;

use super::common;
use super::config::Config;
use super::repository::Repository;
use super::validate_checksums;

/// The [PRIDE](https://www.ebi.ac.uk/pride) proteomics repository at EBI.
pub struct Pride;

impl Repository for Pride {
    async fn get_file_list(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
    ) -> Result<Vec<common::PrideFile>> {
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
            .collect())
    }

    async fn download_file(
        client: &reqwest::Client,
        config: &Config,
        url: &str,
        dest: &Path,
        multi: &MultiProgress,
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
        common::retry_with_backoff(&what, config.max_retries, || {
            let client = client.clone();
            let url = url.to_string();
            let dest = dest.to_path_buf();
            let tmp = tmp.clone();
            let file_name = file_name.clone();
            let multi = multi.clone();
            async move { try_download_once(&client, &url, &dest, &tmp, &file_name, &multi).await }
        })
        .await?;
        Ok(())
    }

    async fn download_dataset(
        client: &reqwest::Client,
        config: &Config,
        accession: &str,
        dest: &Path,
        multi: &MultiProgress,
    ) -> Result<usize> {
        let files = Self::get_file_list(client, config, accession).await?;
        info!("{accession}: {} RAW files listed", files.len());

        let output_dir = dest.join(accession);
        tokio::fs::create_dir_all(&output_dir).await?;

        // Per-dataset file-count bar.
        let dl_bar = multi.add(ProgressBar::new(files.len() as u64));
        dl_bar.set_style(common::count_style());
        dl_bar.set_prefix(format!("{accession} download"));

        // Download missing files with limited concurrency. JoinSet processes
        // results as they complete (not spawn order), so a slow resume doesn't
        // block already-finished skips.
        let sem = Arc::new(tokio::sync::Semaphore::new(config.download_concurrency));
        let mut set = tokio::task::JoinSet::new();
        for file in &files {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .context("semaphore closed unexpectedly")?;
            let client = client.clone();
            let config = config.clone();
            let file_name = file.file_name.clone();
            let url = file.https_url();
            let dest = output_dir.join(&file_name);
            let multi = multi.clone();
            let dl_bar = dl_bar.clone();
            set.spawn(async move {
                let _permit = permit;
                let result = Self::download_file(&client, &config, &url, &dest, &multi).await;
                dl_bar.inc(1);
                (file_name, result)
            });
        }

        let mut failed = Vec::new();
        while let Some(task_result) = set.join_next().await {
            match task_result {
                Ok((_file_name, Ok(()))) => {}
                Ok((file_name, Err(e))) => {
                    warn!("{accession}/{file_name}: download failed: {e:#}");
                    failed.push(file_name);
                }
                Err(e) => {
                    warn!("{accession}: task panicked: {e}");
                    failed.push("unknown".to_string());
                }
            }
        }
        dl_bar.finish_and_clear();

        if !failed.is_empty() {
            anyhow::bail!(
                "{} file(s) failed to download: {}",
                failed.len(),
                failed.join(", ")
            );
        }

        // Validate checksums.
        validate_checksums(client, config, accession, &output_dir, &files, multi).await?;

        Ok(files.len())
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

    let mut resp = resp
        .error_for_status()
        .with_context(|| format!("server returned error for {url}"))?;

    let total = resp.content_length().unwrap_or(0);
    // When resuming with a 206 response, content_length is the *remaining*
    // bytes — add the already-downloaded portion to get the true total.
    let already = if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
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
