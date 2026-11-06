//! Runtime configuration for `pride-rs`.
//!
//! The binary reads an optional TOML config file: the path given with
//! `--config`, or `./pridl.toml` in the current directory when present.
//! Paths inside the config file (like `output`) are resolved relative to the
//! config file's directory, so a project config can be checked into version
//! control next to the data for reproducibility.
//!
//! Each setting is resolved with the following precedence:
//!
//! **CLI flag > environment variable > config file > built-in default**
//!
//! Library users skip file/env loading and construct a [`Config`] directly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::common::cpu_count;

/// Name of the default config file looked up in the current directory.
pub const CONFIG_FILE_NAME: &str = "pridl.toml";

/// Output directory used when nothing else specifies one.
pub const DEFAULT_OUTPUT: &str = "datasets";

/// Env var overriding [`Config::output`].
pub const ENV_OUTPUT: &str = "PRIDL_OUTPUT";
/// Env var overriding [`Config::download_concurrency`].
pub const ENV_DOWNLOAD_CONCURRENCY: &str = "PRIDL_DOWNLOAD_CONCURRENCY";
/// Env var overriding [`Config::validate_concurrency`].
pub const ENV_VALIDATE_CONCURRENCY: &str = "PRIDL_VALIDATE_CONCURRENCY";
/// Env var overriding [`Config::max_retries`].
pub const ENV_MAX_RETRIES: &str = "PRIDL_MAX_RETRIES";

/// Resolved runtime settings, passed explicitly through the library.
///
/// Construct via [`Config::default`] and mutate the fields, or let the
/// binary resolve it from CLI/env/config file with [`Config::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// Directory downloaded files are stored in.
    pub output: PathBuf,
    /// Maximum number of files downloaded in parallel (network/disk-bound).
    pub download_concurrency: usize,
    /// Fixed validation concurrency. `None` uses the adaptive (AIMD) limiter.
    pub validate_concurrency: Option<usize>,
    /// Maximum number of attempts for retryable operations.
    pub max_retries: usize,
}

/// The result of [`Config::resolve`]: the settings plus the download plan
/// listed in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedConfig {
    /// The resolved settings.
    pub config: Config,
    /// Downloads listed in the config file (datasets, then files, then urls).
    pub downloads: Vec<DownloadRequest>,
    /// Path of the config file that was loaded, if any.
    pub config_path: Option<PathBuf>,
}

/// One download action, from the config file or an equivalent subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DownloadRequest {
    /// Download all RAW files of a dataset.
    Dataset(String),
    /// Download a single file by name within a dataset.
    File {
        /// PRIDE accession, e.g. `PXD000001`.
        accession: String,
        /// File name within the dataset, e.g. `sample.raw`.
        file_name: String,
    },
    /// Download a file directly from a URL.
    Url(String),
}

impl DownloadRequest {
    /// Human-readable label for logs and error summaries.
    pub fn label(&self) -> String {
        match self {
            Self::Dataset(accession) => format!("dataset {accession}"),
            Self::File {
                accession,
                file_name,
            } => format!("file {accession}/{file_name}"),
            Self::Url(url) => format!("url {url}"),
        }
    }
}

/// Per-setting overrides coming from the command line (`None` = not given).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CliOverrides {
    /// `--config PATH`
    pub config: Option<PathBuf>,
    /// `-o/--output DIR`
    pub output: Option<PathBuf>,
    /// `--download-concurrency N`
    pub download_concurrency: Option<usize>,
    /// `--validate-concurrency N`
    pub validate_concurrency: Option<usize>,
    /// `--max-retries N`
    pub max_retries: Option<usize>,
}

/// Raw settings as read from env vars (unvalidated).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EnvOverrides {
    output: Option<PathBuf>,
    download_concurrency: Option<usize>,
    validate_concurrency: Option<usize>,
    max_retries: Option<usize>,
}

/// Raw contents of the config file (unvalidated).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FileOverrides {
    output: Option<PathBuf>,
    download_concurrency: Option<usize>,
    validate_concurrency: Option<usize>,
    max_retries: Option<usize>,
    datasets: Vec<String>,
    files: Vec<FileEntry>,
    urls: Vec<String>,
}

/// The on-disk config file format (`pridl.toml`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ConfigFile {
    output: Option<PathBuf>,
    download_concurrency: Option<usize>,
    validate_concurrency: Option<usize>,
    max_retries: Option<usize>,
    #[serde(default)]
    datasets: Vec<String>,
    #[serde(default)]
    files: Vec<FileEntry>,
    #[serde(default)]
    urls: Vec<String>,
}

/// One entry of the `files` list: a single file within a dataset.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct FileEntry {
    accession: String,
    name: String,
}

impl From<ConfigFile> for FileOverrides {
    fn from(file: ConfigFile) -> Self {
        Self {
            output: file.output,
            download_concurrency: file.download_concurrency,
            validate_concurrency: file.validate_concurrency,
            max_retries: file.max_retries,
            datasets: file.datasets,
            files: file.files,
            urls: file.urls,
        }
    }
}

impl FileOverrides {
    /// Maps the file's download lists to download requests, in a fixed order:
    /// datasets, then files, then urls.
    fn downloads(&self) -> Vec<DownloadRequest> {
        self.datasets
            .iter()
            .cloned()
            .map(DownloadRequest::Dataset)
            .chain(self.files.iter().map(|f| DownloadRequest::File {
                accession: f.accession.clone(),
                file_name: f.name.clone(),
            }))
            .chain(self.urls.iter().cloned().map(DownloadRequest::Url))
            .collect()
    }
}

impl EnvOverrides {
    /// Reads the `PRIDL_*` env vars. Unset vars are `None`; set-but-invalid
    /// values are an error so typos don't silently fall back to defaults.
    fn from_env() -> Result<Self> {
        Ok(Self {
            output: env_path(ENV_OUTPUT)?,
            download_concurrency: env_usize(ENV_DOWNLOAD_CONCURRENCY)?,
            validate_concurrency: env_usize(ENV_VALIDATE_CONCURRENCY)?,
            max_retries: env_usize(ENV_MAX_RETRIES)?,
        })
    }
}

impl Default for Config {
    /// Built-in defaults, derived from the machine where applicable.
    fn default() -> Self {
        Self {
            output: PathBuf::from(DEFAULT_OUTPUT),
            download_concurrency: (cpu_count() * 2).min(8),
            validate_concurrency: None,
            max_retries: 5,
        }
    }
}

impl Config {
    /// Resolves the effective configuration and download plan: CLI overrides
    /// win over the `PRIDL_*` env vars, which win over the config file
    /// (`--config PATH`, else `./pridl.toml` when present), which wins over
    /// the built-in defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if an explicit `--config` path does not exist, if the
    /// config file cannot be parsed, or if any explicitly provided value is
    /// invalid (e.g. a non-positive concurrency or retry count).
    pub fn resolve(cli: &CliOverrides) -> Result<ResolvedConfig> {
        Self::resolve_from(Path::new(""), cli)
    }

    /// Like [`Config::resolve`], but looks for the default config file in
    /// `cwd` instead of the process working directory.
    fn resolve_from(cwd: &Path, cli: &CliOverrides) -> Result<ResolvedConfig> {
        let (file, config_path) = load_file(cli.config.as_deref(), cwd)?;
        let config_dir = config_path
            .as_deref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let env = EnvOverrides::from_env()?;
        let config = Self::merge(cli, &file, &env, config_dir.as_deref())?;
        Ok(ResolvedConfig {
            config_path,
            downloads: file.downloads(),
            config,
        })
    }

    /// Pure precedence merge: `cli > env > file > defaults`. Each explicit
    /// value must be positive.
    fn merge(
        cli: &CliOverrides,
        file: &FileOverrides,
        env: &EnvOverrides,
        config_dir: Option<&Path>,
    ) -> Result<Config> {
        let defaults = Self::default();
        // An `output` from the config file is resolved relative to the config
        // file's directory; CLI/env paths stay relative to the current dir.
        let output = match cli.output.clone().or_else(|| env.output.clone()) {
            Some(p) => p,
            None => match &file.output {
                Some(p) => match config_dir {
                    Some(dir) => dir.join(p),
                    None => p.clone(),
                },
                None => defaults.output,
            },
        };
        Ok(Config {
            output,
            download_concurrency: positive(
                "download concurrency",
                cli.download_concurrency
                    .or(env.download_concurrency)
                    .or(file.download_concurrency)
                    .unwrap_or(defaults.download_concurrency),
            )?,
            validate_concurrency: match cli
                .validate_concurrency
                .or(env.validate_concurrency)
                .or(file.validate_concurrency)
            {
                Some(n) => Some(positive("validate concurrency", n)?),
                None => defaults.validate_concurrency,
            },
            max_retries: positive(
                "max retries",
                cli.max_retries
                    .or(env.max_retries)
                    .or(file.max_retries)
                    .unwrap_or(defaults.max_retries),
            )?,
        })
    }
}

/// Finds and parses the config file. Returns the raw values and the path of
/// the file that was loaded (`None` when no config file exists).
fn load_file(cli_path: Option<&Path>, cwd: &Path) -> Result<(FileOverrides, Option<PathBuf>)> {
    let path = match cli_path {
        Some(p) => {
            if !p.is_file() {
                bail!("config file not found: {}", p.display());
            }
            p.to_path_buf()
        }
        None => {
            let default_path = cwd.join(CONFIG_FILE_NAME);
            if default_path.is_file() {
                default_path
            } else {
                return Ok((FileOverrides::default(), None));
            }
        }
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: ConfigFile =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    log::info!("loaded configuration from {}", path.display());
    Ok((FileOverrides::from(parsed), Some(path)))
}

/// Reads a positive `usize` from an env var. Returns `Ok(None)` if unset.
fn env_usize(key: &str) -> Result<Option<usize>> {
    match std::env::var(key) {
        Ok(value) => {
            let n: usize = value.parse().with_context(|| {
                format!("invalid value for {key}: {value:?} (expected a positive integer)")
            })?;
            Ok(Some(positive(key, n)?))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{key} is not valid Unicode")
        }
    }
}

/// Reads a path from an env var. Returns `Ok(None)` if unset.
fn env_path(key: &str) -> Result<Option<PathBuf>> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(PathBuf::from(value))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{key} is not valid Unicode")
        }
    }
}

/// Validates that an explicitly configured concurrency/retry value is positive.
fn positive(source: &str, n: usize) -> Result<usize> {
    if n == 0 {
        bail!("{source} must be greater than 0 (got 0)");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pridl-config-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.output, PathBuf::from(DEFAULT_OUTPUT));
        assert!(c.download_concurrency > 0);
        assert_eq!(c.validate_concurrency, None);
        assert!(c.max_retries > 0);
    }

    #[test]
    fn merge_prefers_cli_over_env_over_file_over_default() {
        let file = FileOverrides {
            output: Some(PathBuf::from("file-data")),
            download_concurrency: Some(2),
            validate_concurrency: None,
            max_retries: Some(3),
            ..Default::default()
        };
        let env = EnvOverrides {
            output: Some(PathBuf::from("env-data")),
            download_concurrency: Some(6),
            validate_concurrency: Some(7),
            max_retries: None,
        };
        let cli = CliOverrides {
            output: Some(PathBuf::from("cli-data")),
            download_concurrency: Some(9),
            ..Default::default()
        };
        let c = Config::merge(&cli, &file, &env, None).unwrap();
        assert_eq!(c.output, PathBuf::from("cli-data"));
        assert_eq!(c.download_concurrency, 9); // cli
        assert_eq!(c.validate_concurrency, Some(7)); // env
        assert_eq!(c.max_retries, 3); // file
    }

    #[test]
    fn file_output_is_relative_to_config_dir() {
        let file = FileOverrides {
            output: Some(PathBuf::from("data")),
            ..Default::default()
        };
        let c = Config::merge(
            &CliOverrides::default(),
            &file,
            &EnvOverrides::default(),
            Some(Path::new("/proj")),
        )
        .unwrap();
        assert_eq!(c.output, PathBuf::from("/proj/data"));
    }

    #[test]
    fn merge_falls_back_to_defaults_when_nothing_set() {
        let c = Config::merge(
            &CliOverrides::default(),
            &FileOverrides::default(),
            &EnvOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn merge_rejects_zero() {
        let cli = CliOverrides {
            download_concurrency: Some(0),
            ..Default::default()
        };
        let err = Config::merge(
            &cli,
            &FileOverrides::default(),
            &EnvOverrides::default(),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("greater than 0"));
    }

    #[test]
    fn config_file_parses_full_schema() {
        let file: ConfigFile = toml::from_str(
            r#"
            output = "data"
            download-concurrency = 3
            validate-concurrency = 2
            max-retries = 10
            datasets = ["PXD000001", "PXD000002"]
            urls = ["https://example.org/x.raw"]

            [[files]]
            accession = "PXD000003"
            name = "sample.raw"
            "#,
        )
        .unwrap();
        let raw = FileOverrides::from(file);
        assert_eq!(raw.output, Some(PathBuf::from("data")));
        assert_eq!(raw.download_concurrency, Some(3));
        assert_eq!(raw.validate_concurrency, Some(2));
        assert_eq!(raw.max_retries, Some(10));
        // Fixed order: datasets, then files, then urls.
        assert_eq!(
            raw.downloads(),
            vec![
                DownloadRequest::Dataset("PXD000001".to_string()),
                DownloadRequest::Dataset("PXD000002".to_string()),
                DownloadRequest::File {
                    accession: "PXD000003".to_string(),
                    file_name: "sample.raw".to_string(),
                },
                DownloadRequest::Url("https://example.org/x.raw".to_string()),
            ]
        );
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        assert!(toml::from_str::<ConfigFile>("download_concurrency = 3\n").is_err());
        assert!(
            toml::from_str::<ConfigFile>("[[files]]\naccession = \"PXD1\"\nbogus = 1\n").is_err()
        );
    }

    #[test]
    fn resolve_reads_config_from_explicit_path() {
        let dir = temp_dir("explicit");
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(
            &path,
            "output = \"data\"\nmax-retries = 9\ndatasets = [\"PXD000001\"]\n",
        )
        .unwrap();
        let resolved = Config::resolve_from(
            Path::new(""),
            &CliOverrides {
                config: Some(path.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        // `output` from the file is relative to the config file's directory.
        assert_eq!(resolved.config.output, dir.join("data"));
        assert_eq!(resolved.config.max_retries, 9);
        assert_eq!(resolved.config_path, Some(path));
        assert_eq!(
            resolved.downloads,
            vec![DownloadRequest::Dataset("PXD000001".to_string())]
        );
    }

    #[test]
    fn resolve_finds_default_config_in_cwd() {
        let dir = temp_dir("cwd");
        std::fs::write(dir.join(CONFIG_FILE_NAME), "max-retries = 7\n").unwrap();
        let resolved = Config::resolve_from(&dir, &CliOverrides::default()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(resolved.config.max_retries, 7);
        assert!(resolved.downloads.is_empty());
        assert_eq!(resolved.config_path, Some(dir.join(CONFIG_FILE_NAME)));
    }

    #[test]
    fn resolve_without_config_file_uses_defaults() {
        let dir = temp_dir("empty");
        let resolved = Config::resolve_from(&dir, &CliOverrides::default()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(resolved.config, Config::default());
        assert!(resolved.downloads.is_empty());
        assert_eq!(resolved.config_path, None);
    }

    #[test]
    fn resolve_errors_on_missing_explicit_config() {
        let err = Config::resolve_from(
            Path::new(""),
            &CliOverrides {
                config: Some(PathBuf::from("/nonexistent/pridl.toml")),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("config file not found"));
    }

    #[test]
    fn resolve_errors_on_malformed_config_file() {
        let dir = temp_dir("bad");
        let path = dir.join(CONFIG_FILE_NAME);
        std::fs::write(&path, "max-retries = \n").unwrap();
        let err = Config::resolve_from(
            Path::new(""),
            &CliOverrides {
                config: Some(path),
                ..Default::default()
            },
        )
        .unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(format!("{err:#}").contains(CONFIG_FILE_NAME));
    }

    #[test]
    fn download_request_labels_are_human_readable() {
        assert_eq!(
            DownloadRequest::Dataset("PXD000001".to_string()).label(),
            "dataset PXD000001"
        );
        assert_eq!(
            DownloadRequest::File {
                accession: "PXD000001".to_string(),
                file_name: "a.raw".to_string()
            }
            .label(),
            "file PXD000001/a.raw"
        );
        assert_eq!(
            DownloadRequest::Url("https://x/y".to_string()).label(),
            "url https://x/y"
        );
    }
}
