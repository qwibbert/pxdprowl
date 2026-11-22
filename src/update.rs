//! Update checks against crates.io.
//!
//! `pxdprowl` is distributed via crates.io (`cargo install pxdprowl`), so the
//! [crates.io API](https://crates.io/api/v1/crates/pxdprowl) is the source of
//! truth for the latest released version. [`check`] compares it with the
//! running version and is used in two places: the `pxdprowl update`
//! subcommand (an explicit, always-run check) and an automatic, rate-limited
//! check at the end of every other command (gated by
//! [`Config::update_check`](crate::config::Config::update_check)) that prints
//! a notice to stderr when a newer release exists. The check is best-effort:
//! network or parse errors never fail a command.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use log::debug;
use serde::Deserialize;

/// crates.io API endpoint describing the `pxdprowl` crate.
pub const CRATES_IO_API_URL: &str = "https://crates.io/api/v1/crates/pxdprowl";

/// How long an update-check request may take before it is abandoned. The
/// automatic check runs after the command's own output, so it must stay
/// snappy; `pxdprowl update` uses the same bound.
pub const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Minimum interval between automatic update checks (the explicit
/// `pxdprowl update` subcommand ignores this).
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The version of the running binary (`CARGO_PKG_VERSION`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A semantic version `major.minor.patch[-pre]` (build metadata after `+` is
/// parsed but ignored, per semver). A pre-release sorts before the plain
/// release (`1.0.0-rc.1 < 1.0.0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl Version {
    /// Parses `s` as `major.minor.patch` with optional `-pre` and `+build`
    /// parts. Returns `None` for anything else (missing/extra numeric parts,
    /// non-numeric parts, empty pre-release, ...).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (core, _) = s.split_once('+').unwrap_or((s, ""));
        let (nums, pre) = match core.split_once('-') {
            Some((nums, pre)) => (nums, Some(pre)),
            None => (core, None),
        };
        if pre == Some("") {
            return None;
        }
        let mut parts = nums.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            pre: pre.map(str::to_string),
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // A release sorts after its pre-releases.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_pre_release(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compares two pre-release strings per semver rules: dot-separated
/// identifiers, numeric identifiers compare numerically and sort before
/// alphanumeric ones, alphanumeric ones compare lexically, and a shorter
/// list sorts before a longer one when it is a prefix of it.
fn cmp_pre_release(a: &str, b: &str) -> Ordering {
    let mut a_ids = a.split('.');
    let mut b_ids = b.split('.');
    loop {
        match (a_ids.next(), b_ids.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// The outcome of an update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// The running version is the latest stable release (or the crate is not
    /// published on crates.io, e.g. a from-source build of a fork).
    UpToDate,
    /// A newer stable release is available on crates.io.
    Available(Version),
}

/// The `crate` object of the crates.io response; all other fields are ignored.
#[derive(Debug, Deserialize)]
struct CratesIoCrate {
    max_stable_version: Option<String>,
}

/// The parts of the crates.io response pxdprowl needs.
#[derive(Debug, Deserialize)]
struct CratesIoResponse {
    #[serde(rename = "crate")]
    krate: CratesIoCrate,
}

/// The latest stable version of `pxdprowl` published on crates.io, or `None`
/// when the crate is not published there (HTTP 404).
///
/// # Errors
///
/// Returns an error if the request fails or times out, if crates.io answers
/// with a non-success status other than 404, or if the response cannot be
/// parsed. The check is a single attempt (no retries): it is best-effort and
/// must not slow down the command it runs after.
pub async fn latest_version(client: &reqwest::Client) -> Result<Option<Version>> {
    latest_version_from(client, CRATES_IO_API_URL).await
}

/// Like [`latest_version`], but queries `url` instead of [`CRATES_IO_API_URL`].
/// The URL is a parameter so tests can point the check at a local mock server.
///
/// # Errors
///
/// Same as [`latest_version`].
pub async fn latest_version_from(client: &reqwest::Client, url: &str) -> Result<Option<Version>> {
    let response = client
        .get(url)
        // crates.io rejects requests without a User-Agent (HTTP 403).
        .header(
            reqwest::header::USER_AGENT,
            concat!("pxdprowl/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(UPDATE_CHECK_TIMEOUT)
        .send()
        .await
        .context("failed to query crates.io for the latest pxdprowl version")?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("crates.io returned HTTP {status} for the latest pxdprowl version");
    }
    let body: CratesIoResponse = response
        .json()
        .await
        .context("failed to parse the crates.io response")?;
    Ok(body
        .krate
        .max_stable_version
        .as_deref()
        .and_then(Version::parse))
}

/// Checks crates.io for a newer release of `pxdprowl`.
///
/// With `force = false` (the automatic check), the check is skipped when the
/// last one happened less than [`UPDATE_CHECK_INTERVAL`] ago, tracked in a
/// small cache file (see [`cache_file`]); `force = true` (the `pxdprowl
/// update` subcommand) always checks. The cache timestamp is refreshed on
/// every performed check.
///
/// # Errors
///
/// Returns an error when the crates.io query fails (see [`latest_version`]).
/// Cache file problems are not errors: they only disable rate limiting.
pub async fn check(client: &reqwest::Client, force: bool) -> Result<UpdateStatus> {
    check_with(
        client,
        force,
        SystemTime::now(),
        cache_file(),
        CRATES_IO_API_URL,
    )
    .await
}

/// Testable core of [`check`]: `now`, `cache` and `url` are injectable.
async fn check_with(
    client: &reqwest::Client,
    force: bool,
    now: SystemTime,
    cache: Option<PathBuf>,
    url: &str,
) -> Result<UpdateStatus> {
    if !force {
        let last = cache.as_deref().and_then(read_last_check);
        if !is_due(last, now) {
            debug!("skipping update check: last check was less than {UPDATE_CHECK_INTERVAL:?} ago");
            return Ok(UpdateStatus::UpToDate);
        }
    }
    let latest = latest_version_from(client, url).await?;
    if let Some(path) = &cache {
        write_last_check(path, now);
    }
    let Some(current) = Version::parse(current_version()) else {
        debug!("could not parse own version {:?}", current_version());
        return Ok(UpdateStatus::UpToDate);
    };
    match latest {
        Some(latest) if latest > current => Ok(UpdateStatus::Available(latest)),
        _ => Ok(UpdateStatus::UpToDate),
    }
}

/// Whether an update check should run at `now`: `last` is `None` (never
/// checked) or at least [`UPDATE_CHECK_INTERVAL`] old. A timestamp in the
/// future (clock skew) counts as fresh, not as due.
fn is_due(last: Option<SystemTime>, now: SystemTime) -> bool {
    match last {
        None => true,
        Some(last) => now
            .duration_since(last)
            .is_ok_and(|age| age >= UPDATE_CHECK_INTERVAL),
    }
}

/// The rate-limit cache file: `$XDG_CACHE_HOME/pxdprowl/last-update-check`, or
/// `~/.cache/pxdprowl/last-update-check` when `XDG_CACHE_HOME` is unset. `None`
/// when neither variable is set (rate limiting is then impossible and every
/// automatic check queries crates.io).
fn cache_file() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|base| base.join("pxdprowl").join("last-update-check"))
}

/// Reads the last-check timestamp from `path`, as stored by [`write_last_check`].
fn read_last_check(path: &Path) -> Option<SystemTime> {
    let text = std::fs::read_to_string(path).ok()?;
    let secs: u64 = text.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Stores `now` as the last-check timestamp in `path` (write-then-rename, so
/// a crash mid-write cannot corrupt the file). Best-effort: failures are
/// logged at debug level and only cost an extra crates.io request next run.
fn write_last_check(path: &Path, now: SystemTime) {
    let Ok(age) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return;
    };
    let result = (|| -> std::io::Result<()> {
        let Some(dir) = path.parent() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cache file has no parent directory",
            ));
        };
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, age.as_secs().to_string())?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        debug!("could not write update-check cache {}: {e}", path.display());
    }
}

/// The notice printed when a newer release is available.
pub fn update_notice(latest: &Version) -> String {
    format!(
        "A new version of pxdprowl is available: {latest} (you have {current}). Update with `cargo install pxdprowl`.",
        current = current_version()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn version_parses_plain_and_suffixed_forms() {
        let v = Version::parse("0.1.1").unwrap();
        assert_eq!((v.major, v.minor, v.patch, v.pre), (0, 1, 1, None));
        let v = Version::parse("1.2.3-rc.1").unwrap();
        assert_eq!(v.pre.as_deref(), Some("rc.1"));
        // Build metadata is accepted but ignored.
        assert_eq!(Version::parse("0.2.0+build.5"), Version::parse("0.2.0"));
        assert_eq!(Version::parse("10.20.30").map(|v| v.major), Some(10));
    }

    #[test]
    fn version_rejects_malformed_input() {
        for bad in [
            "", "1.2", "1.2.3.4", "a.b.c", "1..3", "1.2.x", "1.2.3-", "  ",
        ] {
            assert!(Version::parse(bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn version_orders_numerically_not_lexically() {
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
    }

    #[test]
    fn version_pre_release_sorts_before_release_per_semver() {
        // The ordering example from the semver spec.
        let chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in chain.windows(2) {
            let a = Version::parse(pair[0]).unwrap();
            let b = Version::parse(pair[1]).unwrap();
            assert!(a < b, "{a} should be < {b}");
        }
    }

    #[test]
    fn version_display_round_trips() {
        assert_eq!(Version::parse("0.2.0").unwrap().to_string(), "0.2.0");
        assert_eq!(
            Version::parse("1.0.0-rc.1+build").unwrap().to_string(),
            "1.0.0-rc.1"
        );
    }

    #[test]
    fn crates_io_response_parses_max_stable_version() {
        let body = r#"{"crate": {"max_stable_version": "0.2.0", "max_version": "0.3.0-beta.1"}, "versions": []}"#;
        let parsed: CratesIoResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.krate.max_stable_version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn crates_io_response_tolerates_missing_version() {
        let parsed: CratesIoResponse = serde_json::from_str(r#"{"crate": {}}"#).unwrap();
        assert_eq!(parsed.krate.max_stable_version, None);
    }

    #[test]
    fn is_due_only_after_the_interval() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(is_due(None, now));
        let recent = now - Duration::from_secs(23 * 60 * 60);
        assert!(!is_due(Some(recent), now));
        let old = now - UPDATE_CHECK_INTERVAL - Duration::from_secs(1);
        assert!(is_due(Some(old), now));
        // A timestamp in the future (clock skew) counts as fresh.
        let future = now + Duration::from_secs(60);
        assert!(!is_due(Some(future), now));
    }

    #[test]
    fn cache_timestamp_round_trips() {
        let dir = std::env::temp_dir().join(format!("pxdprowl-update-test-{}", std::process::id()));
        let path = dir.join("pxdprowl").join("last-update-check");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write_last_check(&path, now);
        assert_eq!(read_last_check(&path), Some(now));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_last_check_ignores_garbage() {
        let dir =
            std::env::temp_dir().join(format!("pxdprowl-update-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("last-update-check");
        std::fs::write(&path, "not a timestamp").unwrap();
        assert_eq!(read_last_check(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_notice_names_both_versions_and_the_install_command() {
        let notice = update_notice(&Version::parse("9.9.9").unwrap());
        assert!(notice.contains("9.9.9"), "notice was: {notice}");
        assert!(notice.contains(current_version()), "notice was: {notice}");
        assert!(
            notice.contains("cargo install pxdprowl"),
            "notice was: {notice}"
        );
    }

    /// Binds a local responder that answers every request with `body` and a
    /// fixed status line. Returns the URL and the accept-loop thread handle.
    fn spawn_json_responder(
        status: &str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let status = status.clone();
                std::thread::spawn(move || {
                    // Drain the request headers first, so the client can send
                    // the whole request before we answer and close.
                    let mut buf = [0u8; 4096];
                    let mut received = Vec::new();
                    while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => received.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        (
            format!("http://127.0.0.1:{port}/api/v1/crates/pxdprowl"),
            handle,
        )
    }

    #[tokio::test]
    async fn latest_version_from_parses_a_200_response() {
        let (url, _server) =
            spawn_json_responder("200 OK", r#"{"crate":{"max_stable_version":"99.0.0"}}"#);
        let client = reqwest::Client::new();
        let latest = latest_version_from(&client, &url).await.unwrap();
        assert_eq!(latest, Version::parse("99.0.0"));
    }

    #[tokio::test]
    async fn latest_version_from_maps_404_to_none() {
        let (url, _server) =
            spawn_json_responder("404 Not Found", r#"{"errors":[{"detail":"Not Found"}]}"#);
        let client = reqwest::Client::new();
        assert_eq!(latest_version_from(&client, &url).await.unwrap(), None);
    }

    #[tokio::test]
    async fn latest_version_from_errors_on_500() {
        let (url, _server) = spawn_json_responder("500 Internal Server Error", "{}");
        let client = reqwest::Client::new();
        let err = latest_version_from(&client, &url).await.unwrap_err();
        assert!(format!("{err:#}").contains("500"), "error was: {err:#}");
    }

    #[tokio::test]
    async fn check_with_reports_available_when_latest_is_newer() {
        let (url, _server) =
            spawn_json_responder("200 OK", r#"{"crate":{"max_stable_version":"99.0.0"}}"#);
        let client = reqwest::Client::new();
        let status = check_with(&client, true, SystemTime::now(), None, &url)
            .await
            .unwrap();
        assert_eq!(
            status,
            UpdateStatus::Available(Version::parse("99.0.0").unwrap())
        );
    }

    #[tokio::test]
    async fn check_with_reports_up_to_date_when_latest_is_older() {
        let (url, _server) =
            spawn_json_responder("200 OK", r#"{"crate":{"max_stable_version":"0.0.1"}}"#);
        let client = reqwest::Client::new();
        let status = check_with(&client, true, SystemTime::now(), None, &url)
            .await
            .unwrap();
        assert_eq!(status, UpdateStatus::UpToDate);
    }

    #[tokio::test]
    async fn check_with_skips_the_network_when_the_cache_is_fresh() {
        let dir =
            std::env::temp_dir().join(format!("pxdprowl-update-test-fresh-{}", std::process::id()));
        let cache = dir.join("last-update-check");
        let now = SystemTime::now();
        write_last_check(&cache, now);
        let client = reqwest::Client::new();
        // Port 1 is unroutable: if a request were made, this would error.
        let status = check_with(
            &client,
            false,
            now,
            Some(cache.clone()),
            "http://127.0.0.1:1/x",
        )
        .await
        .unwrap();
        assert_eq!(status, UpdateStatus::UpToDate);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn check_with_runs_when_the_cache_is_stale_and_refreshes_it() {
        let (url, _server) =
            spawn_json_responder("200 OK", r#"{"crate":{"max_stable_version":"0.0.1"}}"#);
        let dir =
            std::env::temp_dir().join(format!("pxdprowl-update-test-stale-{}", std::process::id()));
        let cache = dir.join("last-update-check");
        let now = SystemTime::now();
        let stale = now - UPDATE_CHECK_INTERVAL - Duration::from_secs(60);
        write_last_check(&cache, stale);
        let client = reqwest::Client::new();
        let status = check_with(&client, false, now, Some(cache.clone()), &url)
            .await
            .unwrap();
        assert_eq!(status, UpdateStatus::UpToDate);
        // The cache was refreshed to `now`, so a second run skips the network.
        let last = read_last_check(&cache).unwrap();
        assert!(now.duration_since(last).unwrap() < Duration::from_secs(5));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
