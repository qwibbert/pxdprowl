//! Shared types, constants, and helpers for repository download/validation.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

use anyhow::{Context, Result};
use indicatif::ProgressStyle;
use log::{info, warn};
use serde::Deserialize;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::Config;

/// Number of usable CPU cores (cgroup-aware, affinity-aware).
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .max(1)
}

/// Base URL of the PRIDE archive v3 API.
pub const PRIDE_API_BASE: &str = "https://www.ebi.ac.uk/pride/ws/archive/v3";
/// Prefix of FTP URLs in `publicFileLocations`.
pub const PRIDE_FTP_PREFIX: &str = "ftp://ftp.pride.ebi.ac.uk/";
/// HTTPS equivalent of [`PRIDE_FTP_PREFIX`], used for downloads.
pub const PRIDE_HTTPS_PREFIX: &str = "https://ftp.pride.ebi.ac.uk/";

/// Total timeout for small PRIDE API calls (file lists, checksum TSV). Unlike
/// downloads, these must not hang forever — the client only has `connect_timeout`.
pub const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Connect timeout for downloads.
pub const DOWNLOAD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Per-chunk read timeout for downloads; a stall longer than this aborts the
/// attempt so the retry loop can resume from the `.part` file.
pub const DOWNLOAD_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// A file record from `GET /projects/{accession}/files/all`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrideFile {
    pub file_name: String,
    pub file_category: Option<Category>,
    #[serde(rename = "publicFileLocations", default)]
    pub locations: Vec<FileLocation>,
    pub file_size: Option<u64>,
}

/// The `fileCategory` object of a [`PrideFile`].
#[derive(Debug, Clone, Deserialize)]
pub struct Category {
    pub value: String,
}

/// A single entry of `publicFileLocations` in a [`PrideFile`].
#[derive(Debug, Clone, Deserialize)]
pub struct FileLocation {
    pub name: String,
    pub value: String,
}

impl PrideFile {
    /// Converts the FTP URL from `publicFileLocations` to an HTTPS URL.
    /// Returns an empty string if no FTP location is present.
    pub fn https_url(&self) -> String {
        self.locations
            .iter()
            .find(|l| l.name == "FTP Protocol")
            .map(|l| l.value.replacen(PRIDE_FTP_PREFIX, PRIDE_HTTPS_PREFIX, 1))
            .unwrap_or_default()
    }
}

/// Retries an idempotent operation with exponential backoff. Used for both
/// file downloads (resume via `.part`) and small API GETs (no resume needed).
/// The attempt count comes from [`Config::max_retries`](crate::config::Config::max_retries).
///
/// # Errors
///
/// Returns the last error, with context, once all `max_retries` attempts
/// have failed.
pub async fn retry_with_backoff<T, F, Fut>(what: &str, max_retries: usize, op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    for attempt in 1..=max_retries {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < max_retries => {
                // Saturate instead of panicking on absurdly large max_retries.
                let exp = u32::try_from(attempt - 1).unwrap_or(u32::MAX);
                let backoff = std::time::Duration::from_secs(2u64.saturating_pow(exp));
                warn!(
                    "{what}: attempt {attempt}/{max_retries} failed: {e:#}. Retrying in {backoff:?}"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => {
                return Err(e).context(format!("{what}: all {max_retries} attempts failed"));
            }
        }
    }
    unreachable!("retry loop always returns")
}

/// Byte-level bar: `[====>    ] 1.2 GB/3.4 GB (85.3 MB/s, ETA 0:00:24)`
pub fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
    )
    .expect("hardcoded progress template is valid")
    .progress_chars("=>-")
}

/// Count-level bar: `[====>    ] 3/15 (ETA 0:01:12)`
pub fn count_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix} [{bar:40.green/blue}] {pos}/{len} ({eta})")
        .expect("hardcoded progress template is valid")
        .progress_chars("=>-")
}

/// A dynamically-resizable concurrency limiter. The `limit` field is adjusted
/// at runtime by an AIMD controller based on measured throughput; `in_flight`
/// tracks how many guards are currently held. Waiters sleep on `Notify`.
#[derive(Debug)]
pub struct AdaptiveLimiter {
    max: usize,
    limit: AtomicUsize,
    in_flight: AtomicUsize,
    notify: Notify,
}

impl AdaptiveLimiter {
    /// Creates a limiter with `initial` slots, never exceeding `max`.
    pub fn new(initial: usize, max: usize) -> Self {
        Self {
            max,
            limit: AtomicUsize::new(initial.max(1).min(max)),
            in_flight: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// The maximum number of slots the limit can grow to.
    pub fn max(&self) -> usize {
        self.max
    }

    /// Acquires a slot, waiting until `in_flight < limit`. Returns a guard that
    /// decrements `in_flight` on drop (conc-atomic-ordering: Release on drop,
    /// `AcqRel` on the successful CAS).
    pub async fn acquire(self: &Arc<Self>) -> AdaptiveGuard {
        loop {
            let limit = self.limit.load(Ordering::Relaxed);
            let in_flight = self.in_flight.load(Ordering::Relaxed);
            if in_flight < limit {
                match self.in_flight.compare_exchange(
                    in_flight,
                    in_flight + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return AdaptiveGuard {
                            limiter: self.clone(),
                        };
                    }
                    Err(_) => continue,
                }
            }
            self.notify.notified().await;
        }
    }

    /// AIMD additive increase: limit = min(max, limit + 1).
    pub fn increase(&self) {
        let cur = self.limit.load(Ordering::Relaxed);
        self.limit.store((cur + 1).min(self.max), Ordering::Relaxed);
        self.notify.notify_one();
    }

    /// AIMD multiplicative decrease: limit = max(1, limit / 2).
    pub fn decrease(&self) {
        let cur = self.limit.load(Ordering::Relaxed);
        self.limit.store((cur / 2).max(1), Ordering::Relaxed);
    }

    /// The current limit (number of slots allowed in flight).
    pub fn current(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }
}

/// Guard holding one slot of an [`AdaptiveLimiter`]; releases the slot on drop.
pub struct AdaptiveGuard {
    limiter: Arc<AdaptiveLimiter>,
}

impl Drop for AdaptiveGuard {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, Ordering::Release);
        self.limiter.notify.notify_one();
    }
}

/// Batch-wide concurrency limits shared by every download request of a run.
///
/// Created once per run with [`ConcurrencyLimits::new`] so that
/// [`Config::download_concurrency`] and [`Config::validate_concurrency`] stay
/// global caps even when multiple datasets are downloaded and validated
/// concurrently. Clone the `Arc`s or share one `Arc<ConcurrencyLimits>` to
/// pass the limits into spawned tasks.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConcurrencyLimits {
    /// Global cap on in-flight file downloads across all datasets.
    pub downloads: Arc<Semaphore>,
    /// Global cap on in-flight MD5 validations across all datasets.
    pub validations: Arc<AdaptiveLimiter>,
    /// Aggregate bytes hashed so far, sampled by the AIMD controller.
    pub(crate) bytes_hashed: Arc<AtomicU64>,
    /// Whether the validation limiter is adaptive (AIMD) or fixed.
    adaptive: bool,
    /// Handle of the AIMD controller task, spawned lazily by
    /// [`ConcurrencyLimits::start_aimd`] and aborted on drop.
    aimd: Mutex<Option<JoinHandle<()>>>,
}

impl ConcurrencyLimits {
    /// Creates the shared limits for a run from `config`: one download
    /// semaphore with `download_concurrency` permits, and one validation
    /// limiter — fixed at `validate_concurrency`, or adaptive (AIMD) up to
    /// the CPU count when unset.
    pub fn new(config: &Config) -> Self {
        let (initial, max) = match config.validate_concurrency {
            Some(n) => (n, n),
            None => (cpu_count().min(16), cpu_count()),
        };
        Self {
            downloads: Arc::new(Semaphore::new(config.download_concurrency)),
            validations: Arc::new(AdaptiveLimiter::new(initial, max)),
            bytes_hashed: Arc::new(AtomicU64::new(0)),
            adaptive: config.validate_concurrency.is_none(),
            aimd: Mutex::new(None),
        }
    }

    /// Spawns the AIMD controller for [`ConcurrencyLimits::validations`] once;
    /// later calls are no-ops. Does nothing when a fixed validation
    /// concurrency was configured. The controller runs until the limits are
    /// dropped, adjusting the shared limiter based on the aggregate hash
    /// throughput of all concurrent validations.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime (`tokio::spawn` requires one).
    pub fn start_aimd(&self) {
        if !self.adaptive {
            return;
        }
        // The lock is only held for the (synchronous) spawn, never across an await.
        let mut guard = self.aimd.lock().expect("aimd mutex poisoned");
        if guard.is_some() {
            return;
        }
        let ctrl = self.validations.clone();
        let ctrl_bytes = self.bytes_hashed.clone();
        *guard = Some(tokio::spawn(async move {
            // On flat throughput, probe upward every PROBE_EVERY_FLAT-th sample
            // so the limit can recover when external IO contention ends.
            const PROBE_EVERY_FLAT: u32 = 5;
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
        }));
    }
}

impl Drop for ConcurrencyLimits {
    fn drop(&mut self) {
        if let Some(handle) = self.aimd.lock().expect("aimd mutex poisoned").take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_with_locations(locations: Vec<FileLocation>) -> PrideFile {
        PrideFile {
            file_name: "sample.raw".to_string(),
            file_category: Some(Category {
                value: "RAW".to_string(),
            }),
            locations,
            file_size: Some(42),
        }
    }

    #[test]
    fn https_url_rewrites_ftp_prefix() {
        let file = file_with_locations(vec![FileLocation {
            name: "FTP Protocol".to_string(),
            value: format!("{PRIDE_FTP_PREFIX}pride/data/archive/2024/sample.raw"),
        }]);
        assert_eq!(
            file.https_url(),
            format!("{PRIDE_HTTPS_PREFIX}pride/data/archive/2024/sample.raw")
        );
    }

    #[test]
    fn https_url_empty_without_ftp_location() {
        let file = file_with_locations(vec![FileLocation {
            name: "Aspera".to_string(),
            value: "aspera://example/sample.raw".to_string(),
        }]);
        assert_eq!(file.https_url(), "");
    }

    #[test]
    fn pride_file_deserializes_camel_case_json() {
        let json = r#"{
            "fileName": "sample.raw",
            "fileCategory": {"value": "RAW"},
            "publicFileLocations": [{"name": "FTP Protocol", "value": "ftp://ftp.pride.ebi.ac.uk/x.raw"}],
            "fileSize": 123
        }"#;
        let file: PrideFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.file_name, "sample.raw");
        assert_eq!(file.file_category.unwrap().value, "RAW");
        assert_eq!(file.locations.len(), 1);
        assert_eq!(file.file_size, Some(123));
    }

    #[test]
    fn concurrency_limits_use_config_caps() {
        let config = Config {
            download_concurrency: 3,
            validate_concurrency: Some(2),
            ..Config::default()
        };
        let limits = ConcurrencyLimits::new(&config);
        assert_eq!(limits.downloads.available_permits(), 3);
        assert_eq!(limits.validations.current(), 2);
        assert_eq!(limits.validations.max(), 2);
        assert!(!limits.adaptive);
    }

    #[test]
    fn concurrency_limits_default_to_adaptive_validation() {
        let limits = ConcurrencyLimits::new(&Config::default());
        assert!(limits.adaptive);
        assert_eq!(limits.validations.max(), cpu_count());
        assert_eq!(limits.validations.current(), cpu_count().min(16));
    }

    #[tokio::test]
    async fn start_aimd_skipped_for_fixed_validation_concurrency() {
        let config = Config {
            validate_concurrency: Some(4),
            ..Config::default()
        };
        let limits = ConcurrencyLimits::new(&config);
        limits.start_aimd();
        assert!(limits.aimd.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn start_aimd_spawns_controller_only_once() {
        let limits = ConcurrencyLimits::new(&Config::default());
        limits.start_aimd();
        let first = limits.aimd.lock().unwrap().as_ref().map(|h| h.id());
        limits.start_aimd();
        let second = limits.aimd.lock().unwrap().as_ref().map(|h| h.id());
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn shared_validation_limiter_caps_in_flight_acquires() {
        let config = Config {
            validate_concurrency: Some(2),
            ..Config::default()
        };
        let limits = Arc::new(ConcurrencyLimits::new(&config));
        let g1 = limits.validations.acquire().await;
        let g2 = limits.validations.acquire().await;
        // Third acquire must wait until a guard is dropped.
        let third = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            limits.validations.acquire(),
        )
        .await;
        assert!(third.is_err());
        drop(g1);
        let third = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            limits.validations.acquire(),
        )
        .await;
        assert!(third.is_ok());
        drop(g2);
        drop(third.unwrap());
    }
}
