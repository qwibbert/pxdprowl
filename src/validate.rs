//! MD5 checksum validation of downloaded files against the PRIDE checksum TSV.
//!
//! MD5 is CPU-bound, so files are hashed in parallel, capped by the shared
//! [`common::ConcurrencyLimits`] (fixed at [`Config::validate_concurrency`],
//! or adaptive via AIMD when unset). [`validate_file`] validates a single
//! file — used to validate each file inline as soon as its download completes
//! — while [`validate_checksums`] validates a whole dataset in one call.

use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar};
use log::{info, warn};
use md5::{Digest, Md5};

use super::common;
use super::config::Config;

/// Fetches and parses the PRIDE checksum TSV (`File-Name\tFile-MD5Checksum\tFile-Size`)
/// for `accession` into a map of file name to expected MD5 hex digest.
///
/// # Errors
///
/// Returns an error if the TSV cannot be fetched after `config.max_retries`
/// attempts or the response cannot be read.
pub async fn fetch_checksums(
    client: &reqwest::Client,
    config: &Config,
    accession: &str,
) -> Result<HashMap<String, String>> {
    let url = format!("{}/files/checksum/{accession}", common::PRIDE_API_BASE);
    info!("{accession}: fetching checksums...");
    let what = format!("fetch checksums for {accession}");
    let body = common::retry_with_backoff(&what, config.max_retries, || {
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
    }
    Ok(checksums)
}

/// Validates one downloaded file: hashes `output_dir/file_name` with MD5 in a
/// blocking task (async-spawn-blocking), gated by the shared validation
/// limiter, and compares against `expected` (case-insensitive). On mismatch
/// the file is deleted so the next run re-downloads it instead of skipping
/// it via the exists() check.
///
/// Call [`common::ConcurrencyLimits::start_aimd`] once before validating a
/// batch so the adaptive limiter's controller is running.
///
/// # Errors
///
/// Returns an error if the MD5 cannot be computed or the checksum mismatches.
pub async fn validate_file(
    limits: &Arc<common::ConcurrencyLimits>,
    accession: &str,
    output_dir: &Path,
    rel_path: &str,
    expected: &str,
    file_size: u64,
    multi: &MultiProgress,
) -> Result<()> {
    let path = output_dir.join(rel_path);

    // Per-file byte bar for MD5 reading progress.
    let md5_pb = multi.add(ProgressBar::new(file_size));
    md5_pb.set_style(common::bytes_style());
    md5_pb.set_prefix(format!("MD5 {rel_path}"));

    let guard = limits.validations.acquire().await;
    let md5_result = {
        let path = path.clone();
        let pb = md5_pb.clone();
        let counter = limits.bytes_hashed.clone();
        match tokio::task::spawn_blocking(move || compute_md5(&path, &pb, &counter)).await {
            Ok(r) => r,
            Err(e) => Err(e).context("md5 task panicked"),
        }
    };
    drop(guard);
    md5_pb.finish_and_clear();

    let actual = match md5_result {
        Ok(actual) => actual,
        Err(e) => {
            warn!("{accession}/{rel_path}: failed to compute MD5: {e:#}");
            return Err(e).context(format!("{accession}/{rel_path}: failed to compute MD5"));
        }
    };

    if actual.eq_ignore_ascii_case(expected) {
        info!("{accession}/{rel_path}: checksum OK");
        return Ok(());
    }

    warn!("{accession}/{rel_path}: checksum mismatch (expected {expected}, got {actual})");
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {
            info!("{accession}/{rel_path}: removed corrupt file, will re-download on next run")
        }
        Err(e) => warn!("{accession}/{rel_path}: failed to remove corrupt file: {e}"),
    }
    bail!("{accession}/{rel_path}: checksum mismatch (expected {expected}, got {actual})")
}

/// Validates a downloaded file by size. Used for repositories that publish no
/// checksums (MassIVE, iProX): the listing's reported size is compared to the
/// on-disk size. On mismatch the file is deleted so the next run re-downloads
/// it instead of skipping it via the exists() check.
///
/// # Errors
///
/// Returns an error if the file cannot be statted or its size differs from
/// `expected`.
pub async fn validate_size(label: &str, path: &Path, expected: u64) -> Result<()> {
    let actual = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    if actual == expected {
        info!("{label}: size OK ({actual} bytes)");
        return Ok(());
    }
    warn!("{label}: size mismatch (expected {expected}, got {actual})");
    match tokio::fs::remove_file(path).await {
        Ok(()) => {
            info!("{label}: removed incomplete file, will re-download on next run")
        }
        Err(e) => warn!("{label}: failed to remove incomplete file: {e}"),
    }
    bail!("{label}: size mismatch (expected {expected}, got {actual})")
}

/// Fetches the PRIDE checksum TSV and validates MD5 of every file in `files`
/// in parallel (see [`validate_file`]), capped by the shared `limits` so the
/// cap stays global when several datasets validate at once.
///
/// # Errors
///
/// Returns an error if the checksum TSV cannot be fetched after
/// `config.max_retries` attempts, or if any file's checksum mismatches or
/// cannot be computed.
pub async fn validate_checksums(
    client: &reqwest::Client,
    config: &Config,
    limits: &Arc<common::ConcurrencyLimits>,
    accession: &str,
    output_dir: &Path,
    files: &[common::RemoteFile],
    multi: &MultiProgress,
) -> Result<()> {
    let checksums = fetch_checksums(client, config, accession).await?;
    if checksums.is_empty() {
        return Ok(());
    }

    // Per-dataset validation count bar.
    let val_bar = multi.add(ProgressBar::new(files.len() as u64));
    val_bar.set_style(common::count_style());
    val_bar.set_prefix(format!("{accession} validating"));
    limits.start_aimd();

    // Spawn one task per file; each task acquires a slot of the shared
    // limiter inside validate_file and offloads the CPU-bound MD5 to
    // spawn_blocking.
    let mut set = tokio::task::JoinSet::new();
    for file in files {
        let rel_path = file.relative_path.clone();
        let Some(expected) = checksums.get(&file.name).cloned() else {
            warn!("{accession}/{rel_path}: no checksum found, skipping");
            val_bar.inc(1);
            continue;
        };
        let path = output_dir.join(&rel_path);
        let file_size = file
            .size
            .unwrap_or_else(|| std::fs::metadata(&path).map_or(0, |m| m.len()));

        let limits = limits.clone();
        let output_dir = output_dir.to_path_buf();
        let multi = multi.clone();
        let val_bar = val_bar.clone();
        let accession = accession.to_string();
        set.spawn(async move {
            let result = validate_file(
                &limits,
                &accession,
                &output_dir,
                &rel_path,
                &expected,
                file_size,
                &multi,
            )
            .await;
            val_bar.inc(1);
            (rel_path, result)
        });
    }

    // Collect results as tasks complete (completion order, not spawn order).
    // validate_file already logged the per-file specifics.
    let mut failures = Vec::new();
    while let Some(task_result) = set.join_next().await {
        match task_result {
            Ok((_file_name, Ok(()))) => {}
            Ok((file_name, Err(_))) => {
                failures.push(file_name);
            }
            Err(e) => {
                warn!("{accession}: validation task panicked: {e}");
                failures.push("unknown".to_string());
            }
        }
    }
    val_bar.finish_and_clear();

    if !failures.is_empty() {
        bail!(
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

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pxdprowl-validate-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // md5("hello")
    const MD5_HELLO: &str = "5d41402abc4b2a76b9719d911017c592";

    #[tokio::test]
    async fn validate_file_accepts_matching_checksum() {
        let dir = temp_dir("ok");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let limits = Arc::new(common::ConcurrencyLimits::new(&Config::default()));
        let multi = MultiProgress::new();
        validate_file(&limits, "PXD000001", &dir, "a.raw", MD5_HELLO, 5, &multi)
            .await
            .unwrap();
        assert!(dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn validate_file_accepts_uppercase_checksum() {
        let dir = temp_dir("upper");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let limits = Arc::new(common::ConcurrencyLimits::new(&Config::default()));
        let multi = MultiProgress::new();
        validate_file(
            &limits,
            "PXD000001",
            &dir,
            "a.raw",
            &MD5_HELLO.to_uppercase(),
            5,
            &multi,
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn validate_file_deletes_file_and_errors_on_mismatch() {
        let dir = temp_dir("mismatch");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let limits = Arc::new(common::ConcurrencyLimits::new(&Config::default()));
        let multi = MultiProgress::new();
        let err = validate_file(
            &limits,
            "PXD000001",
            &dir,
            "a.raw",
            &"0".repeat(32),
            5,
            &multi,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(!dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn validate_file_errors_on_missing_file() {
        let dir = temp_dir("missing");
        let limits = Arc::new(common::ConcurrencyLimits::new(&Config::default()));
        let multi = MultiProgress::new();
        let err = validate_file(&limits, "PXD000001", &dir, "gone.raw", MD5_HELLO, 5, &multi)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failed to compute MD5"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn validate_size_accepts_matching_size() {
        let dir = temp_dir("size-ok");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        validate_size("PXD000001", &dir.join("a.raw"), 5).await.unwrap();
        assert!(dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn validate_size_deletes_file_and_errors_on_mismatch() {
        let dir = temp_dir("size-mismatch");
        std::fs::write(dir.join("a.raw"), b"hello").unwrap();
        let err = validate_size("PXD000001", &dir.join("a.raw"), 999)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("size mismatch"));
        assert!(!dir.join("a.raw").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
