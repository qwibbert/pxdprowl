//! Runtime configuration for `pride-rs`.
//!
//! Settings are resolved per field with the following precedence:
//!
//! **CLI flag > environment variable > config file > built-in default**
//!
//! The optional config file ([`CONFIG_FILE_NAME`]) is looked up in the
//! output directory (the root of where the data is stored), so it can be
//! checked into version control next to the data for reproducibility.
//! File loading is only wired up in the binary (`cli::run`); library users
//! construct a [`Config`] directly.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::common::cpu_count;

/// Name of the optional TOML config file looked up in the output directory.
pub const CONFIG_FILE_NAME: &str = "pridl.toml";

/// Env var overriding [`Config::download_concurrency`].
pub const ENV_DOWNLOAD_CONCURRENCY: &str = "PRIDL_DOWNLOAD_CONCURRENCY";
/// Env var overriding [`Config::validate_concurrency`].
pub const ENV_VALIDATE_CONCURRENCY: &str = "PRIDL_VALIDATE_CONCURRENCY";
/// Env var overriding [`Config::max_retries`].
pub const ENV_MAX_RETRIES: &str = "PRIDL_MAX_RETRIES";

/// Resolved runtime settings, passed explicitly through the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Maximum number of files downloaded in parallel (network/disk-bound).
    pub download_concurrency: usize,
    /// Fixed validation concurrency. `None` uses the adaptive (AIMD) limiter.
    pub validate_concurrency: Option<usize>,
    /// Maximum number of attempts for retryable operations.
    pub max_retries: usize,
}

/// Per-setting overrides coming from the command line (`None` = not given).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CliOverrides {
    /// `--download-concurrency`
    pub download_concurrency: Option<usize>,
    /// `--validate-concurrency`
    pub validate_concurrency: Option<usize>,
    /// `--max-retries`
    pub max_retries: Option<usize>,
}

/// Raw (unvalidated) settings as read from a config file or env vars.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RawOverrides {
    download_concurrency: Option<usize>,
    validate_concurrency: Option<usize>,
    max_retries: Option<usize>,
}

/// The on-disk config file format (`pridl.toml` in the output directory).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ConfigFile {
    download_concurrency: Option<usize>,
    validate_concurrency: Option<usize>,
    max_retries: Option<usize>,
}

impl Config {
    /// Built-in defaults, derived from the machine where applicable.
    pub fn defaults() -> Self {
        Self {
            download_concurrency: (cpu_count() * 2).min(8),
            validate_concurrency: None,
            max_retries: 5,
        }
    }

    /// Resolves the effective configuration: CLI overrides win over the
    /// `PRIDL_*` env vars, which win over `pridl.toml` in `dir` (if present),
    /// which wins over the built-in defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if `pridl.toml` exists but cannot be parsed, or if any
    /// explicitly provided value is not a positive integer.
    pub fn resolve(dir: &Path, cli: CliOverrides) -> Result<Self> {
        let file = RawOverrides::from_file(dir)?;
        let env = RawOverrides::from_env()?;
        Self::merge(Self::defaults(), file, env, cli)
    }

    /// Pure precedence merge: `cli > env > file > defaults`. Each explicit
    /// value must be positive.
    fn merge(
        defaults: Config,
        file: RawOverrides,
        env: RawOverrides,
        cli: CliOverrides,
    ) -> Result<Self> {
        Ok(Config {
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

impl RawOverrides {
    /// Reads `pridl.toml` from `dir` if it exists. A missing file is not an
    /// error; a malformed one is.
    fn from_file(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE_NAME);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        let file: ConfigFile = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        log::info!("loaded configuration from {}", path.display());
        Ok(Self {
            download_concurrency: file.download_concurrency,
            validate_concurrency: file.validate_concurrency,
            max_retries: file.max_retries,
        })
    }

    /// Reads the `PRIDL_*` env vars. Unset vars are `None`; set-but-invalid
    /// vars are an error so typos don't silently fall back to defaults.
    fn from_env() -> Result<Self> {
        Ok(Self {
            download_concurrency: env_usize(ENV_DOWNLOAD_CONCURRENCY)?,
            validate_concurrency: env_usize(ENV_VALIDATE_CONCURRENCY)?,
            max_retries: env_usize(ENV_MAX_RETRIES)?,
        })
    }
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

    fn defaults() -> Config {
        Config {
            download_concurrency: 4,
            validate_concurrency: None,
            max_retries: 5,
        }
    }

    #[test]
    fn defaults_are_positive() {
        let c = Config::defaults();
        assert!(c.download_concurrency > 0);
        assert!(c.max_retries > 0);
        assert_eq!(c.validate_concurrency, None);
    }

    #[test]
    fn merge_prefers_cli_over_env_over_file_over_default() {
        let file = RawOverrides {
            download_concurrency: Some(2),
            validate_concurrency: None,
            max_retries: Some(3),
        };
        let env = RawOverrides {
            download_concurrency: Some(6),
            validate_concurrency: Some(7),
            max_retries: None,
        };
        let cli = CliOverrides {
            download_concurrency: Some(9),
            validate_concurrency: None,
            max_retries: None,
        };
        let c = Config::merge(defaults(), file, env, cli).unwrap();
        assert_eq!(c.download_concurrency, 9); // cli
        assert_eq!(c.validate_concurrency, Some(7)); // env
        assert_eq!(c.max_retries, 3); // file
    }

    #[test]
    fn merge_falls_back_to_defaults_when_nothing_set() {
        let c = Config::merge(
            defaults(),
            RawOverrides::default(),
            RawOverrides::default(),
            CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(c, defaults());
    }

    #[test]
    fn merge_rejects_zero() {
        let cli = CliOverrides {
            download_concurrency: Some(0),
            validate_concurrency: None,
            max_retries: None,
        };
        let err = Config::merge(
            defaults(),
            RawOverrides::default(),
            RawOverrides::default(),
            cli,
        )
        .unwrap_err();
        assert!(err.to_string().contains("greater than 0"));
    }

    #[test]
    fn config_file_parses_kebab_case_keys() {
        let file: ConfigFile = toml::from_str(
            "download-concurrency = 3\nvalidate-concurrency = 2\nmax-retries = 10\n",
        )
        .unwrap();
        assert_eq!(file.download_concurrency, Some(3));
        assert_eq!(file.validate_concurrency, Some(2));
        assert_eq!(file.max_retries, Some(10));
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        let err = toml::from_str::<ConfigFile>("download_concurrency = 3\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn config_file_rejects_non_positive_values_at_merge() {
        let file: ConfigFile = toml::from_str("max-retries = 0\n").unwrap();
        let raw = RawOverrides {
            download_concurrency: file.download_concurrency,
            validate_concurrency: file.validate_concurrency,
            max_retries: file.max_retries,
        };
        assert!(
            Config::merge(
                defaults(),
                raw,
                RawOverrides::default(),
                CliOverrides::default()
            )
            .is_err()
        );
    }

    #[test]
    fn resolve_loads_pridl_toml_from_output_dir() {
        let dir = std::env::temp_dir().join(format!("pridl-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE_NAME), "max-retries = 9\n").unwrap();
        let c = Config::resolve(&dir, CliOverrides::default()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(c.max_retries, 9);
    }

    #[test]
    fn resolve_ignores_missing_config_file() {
        let dir =
            std::env::temp_dir().join(format!("pridl-config-test-missing-{}", std::process::id()));
        let c = Config::resolve(&dir, CliOverrides::default()).unwrap();
        assert_eq!(c, Config::defaults());
    }

    #[test]
    fn resolve_errors_on_malformed_config_file() {
        let dir =
            std::env::temp_dir().join(format!("pridl-config-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE_NAME), "max-retries = \n").unwrap();
        let err = Config::resolve(&dir, CliOverrides::default()).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(format!("{err:#}").contains(CONFIG_FILE_NAME));
    }
}
