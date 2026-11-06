//! MD5 checksum validation of downloaded files against the PRIDE checksum TSV.

use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar};
use log::{info, warn};
use md5::{Digest, Md5};
use tokio::time::Instant;

use super::common;

/// Downloads the PRIDE checksum TSV and validates MD5 of each downloaded file
/// in parallel (limited by `VALIDATE_CONCURRENCY`), since MD5 is CPU-bound.
/// Files with a mismatching checksum are deleted so the next run re-downloads
/// them.
///
/// # Errors
///
/// Returns an error if the checksum TSV cannot be fetched after `MAX_RETRIES`,
/// or if any file's checksum mismatches or cannot be computed.
pub async fn validate_checksums(
    client: &reqwest::Client,
    accession: &str,
    output_dir: &Path,
    files: &[common::PrideFile],
    multi: &MultiProgress,
) -> Result<()> {
    let url = format!("{}/files/checksum/{accession}", common::PRIDE_API_BASE);
    info!("{accession}: fetching checksums...");
    let what = format!("fetch checksums for {accession}");
    let body = common::retry_with_backoff(&what, || {
        let client = client.clone();
        let url = url.clone();
        async move {
            client
                .get(&url)
                .header("accept", "text/plain")
                .timeout(common::API_TIMEOUT)
                .send()
                .await
                .with_context(|| format!("failed to fetch checksums for {accession}"))?
                .text()
                .await
                .context("failed to read checksum response")
        }
    })
    .await?;

    let checksums = parse_checksum_tsv(&body);
    if checksums.is_empty() {
        warn!("{accession}: checksum file was empty, skipping validation");
        return Ok(());
    }

    // Per-dataset validation count bar.
    let val_bar = multi.add(ProgressBar::new(files.len() as u64));
    val_bar.set_style(common::count_style());
    val_bar.set_prefix(format!("{accession} validating"));

    // Concurrency: fixed override via VALIDATE_CONCURRENCY, otherwise AIMD
    // (adaptive limiter starting at min(cpu, 16), max cpu_count). The AIMD
    // controller samples aggregate hash throughput and adjusts the limit.
    let bytes_hashed = Arc::new(AtomicU64::new(0));
    let (limiter, fixed_limit): (Arc<common::AdaptiveLimiter>, Option<usize>) =
        match *common::VALIDATE_CONCURRENCY {
            Some(n) => (Arc::new(common::AdaptiveLimiter::new(n, n)), Some(n)),
            None => (
                Arc::new(common::AdaptiveLimiter::new(
                    common::cpu_count().min(16),
                    common::cpu_count(),
                )),
                None,
            ),
        };

    // Spawn AIMD controller unless a fixed limit was requested.
    let aimd_handle = if fixed_limit.is_none() {
        // On flat throughput, probe upward every PROBE_EVERY_FLAT-th sample
        // so the limit can recover when external IO contention ends.
        const PROBE_EVERY_FLAT: u32 = 5;
        let ctrl = limiter.clone();
        let ctrl_bytes = bytes_hashed.clone();
        Some(tokio::spawn(async move {
            let mut prev_throughput = 0.0f64;
            let mut last_bytes = 0u64;
            let mut last = Instant::now();
            let mut flat_count = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let now = Instant::now();
                let cur = ctrl_bytes.load(Ordering::Relaxed);
                let dt = now.duration_since(last).as_secs_f64();
                let throughput = (cur.saturating_sub(last_bytes)) as f64 / dt.max(0.001);
                if prev_throughput > 0.0 {
                    if throughput > prev_throughput * 1.05 {
                        // Rising — more parallelism helps, haven't found ceiling yet.
                        ctrl.increase();
                        flat_count = 0;
                    } else if throughput < prev_throughput * 0.95 {
                        // Dropping — overshot the IO ceiling, back off.
                        ctrl.decrease();
                        flat_count = 0;
                    } else {
                        // Flat (within ±5%) — at the ceiling, hold but probe
                        // upward periodically to detect freed-up IO bandwidth.
                        flat_count += 1;
                        if flat_count >= PROBE_EVERY_FLAT {
                            ctrl.increase();
                            flat_count = 0;
                        }
                    }
                }
                info!(
                    "validate AIMD: limit={}/{} throughput={:.1} MB/s",
                    ctrl.current(),
                    ctrl.max(),
                    throughput / 1_000_000.0
                );
                prev_throughput = throughput;
                last_bytes = cur;
                last = now;
            }
        }))
    } else {
        None
    };

    // Spawn one task per file, gated by the adaptive limiter. Each task
    // offloads the CPU-bound MD5 to spawn_blocking (async-spawn-blocking).
    let mut set = tokio::task::JoinSet::new();

    for file in files {
        let file_name = file.file_name.clone();
        let Some(expected) = checksums.get(&file_name).cloned() else {
            warn!("{accession}/{file_name}: no checksum found, skipping");
            val_bar.inc(1);
            continue;
        };
        let path = output_dir.join(&file_name);
        let file_size = file
            .file_size
            .unwrap_or_else(|| std::fs::metadata(&path).map_or(0, |m| m.len()));

        // Per-file byte bar for MD5 reading progress.
        let md5_pb = multi.add(ProgressBar::new(file_size));
        md5_pb.set_style(common::bytes_style());
        md5_pb.set_prefix(format!("MD5 {file_name}"));

        let guard = limiter.acquire().await;
        let val_bar = val_bar.clone();
        let bytes_hashed = bytes_hashed.clone();
        set.spawn(async move {
            let _guard = guard;
            let md5_result = {
                let path = path.clone();
                let pb = md5_pb.clone();
                let counter = bytes_hashed.clone();
                match tokio::task::spawn_blocking(move || compute_md5(&path, &pb, &counter)).await {
                    Ok(r) => r,
                    Err(e) => Err(e).context("md5 task panicked"),
                }
            };
            md5_pb.finish_and_clear();
            val_bar.inc(1);
            (file_name, expected, md5_result)
        });
    }

    // Collect results as tasks complete (completion order, not spawn order).
    let mut failures = Vec::new();
    while let Some(task_result) = set.join_next().await {
        match task_result {
            Ok((file_name, expected, Ok(actual))) if actual.eq_ignore_ascii_case(&expected) => {
                info!("{file_name}: checksum OK");
            }
            Ok((file_name, expected, Ok(actual))) => {
                warn!("{file_name}: checksum mismatch (expected {expected}, got {actual})");
                // Delete the corrupt file so the next run re-downloads it
                // instead of skipping it via the exists() check.
                let corrupt_path = output_dir.join(&file_name);
                match tokio::fs::remove_file(&corrupt_path).await {
                    Ok(()) => {
                        info!("{file_name}: removed corrupt file, will re-download on next run");
                    }
                    Err(e) => warn!("{file_name}: failed to remove corrupt file: {e}"),
                }
                failures.push(file_name);
            }
            Ok((file_name, _, Err(e))) => {
                warn!("{file_name}: failed to compute MD5: {e}");
                failures.push(file_name);
            }
            Err(e) => {
                warn!("{accession}: validation task panicked: {e}");
                failures.push("unknown".to_string());
            }
        }
    }
    val_bar.finish_and_clear();
    if let Some(h) = aimd_handle {
        h.abort();
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "checksum validation failed for {} file(s): {}",
            failures.len(),
            failures.join(", ")
        );
    }
    info!("{accession}: all checksums validated");
    Ok(())
}

/// Parses the PRIDE checksum TSV (`File-Name\tFile-MD5Checksum\tFile-Size`).
fn parse_checksum_tsv(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::with_capacity(body.lines().count());
    for (i, line) in body.lines().enumerate() {
        if i == 0 {
            continue; // header
        }
        let mut parts = line.splitn(3, '\t');
        if let (Some(name), Some(md5)) = (parts.next(), parts.next()) {
            map.insert(name.to_string(), md5.to_string());
        }
    }
    map
}

/// Computes the MD5 hex digest of a file, reporting byte progress to `pb` and
/// aggregate throughput to `counter` (for the AIMD controller).
fn compute_md5(path: &Path, pb: &ProgressBar, counter: &AtomicU64) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024]; // 1 MB chunks, heap-allocated
    loop {
        let n = file.read(&mut buf).context("failed to read file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        pb.inc(n as u64);
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_checksum_tsv_skips_header_and_maps_name_to_md5() {
        let body = "File-Name\tFile-MD5Checksum\tFile-Size\n\
                    sample1.raw\t0123456789abcdef0123456789abcdef\t100\n\
                    sample2.raw\tffffffffffffffffffffffffffffffff\t200\n";
        let map = parse_checksum_tsv(body);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("sample1.raw").map(String::as_str),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            map.get("sample2.raw").map(String::as_str),
            Some("ffffffffffffffffffffffffffffffff")
        );
    }

    #[test]
    fn parse_checksum_tsv_ignores_malformed_lines() {
        let body = "File-Name\tFile-MD5Checksum\tFile-Size\n\
                    broken-line-without-tabs\n\
                    ok.raw\tabc\t1\n";
        let map = parse_checksum_tsv(body);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("ok.raw").map(String::as_str), Some("abc"));
    }

    #[test]
    fn parse_checksum_tsv_empty_body() {
        assert!(parse_checksum_tsv("").is_empty());
    }
}
