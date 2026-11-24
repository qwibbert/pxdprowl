#![doc = include_str!("../README.md")]

//! `pxdprowl` — download datasets and files from the
//! [PRIDE](https://www.ebi.ac.uk/pride) proteomics archive.
//!
//! The crate is split into a library (this module tree) and a thin binary
//! entry point (`src/main.rs`). All logic lives here so it is testable and
//! reusable. Library users should start from [`Client`]; the lower-level
//! modules stay public for finer control.

// Native `async fn` in traits is the recommended style (stable since 1.75);
// the `Send`-bound caveat does not affect this crate because every spawned
// future is `Send` at the concrete impl site.
#![allow(async_fn_in_trait)]

pub mod cli;
pub mod client;
pub mod common;
pub mod config;
pub mod iprox;
pub mod massive;
pub mod pride;
pub mod proteomexchange;
pub mod repository;
pub mod update;
pub mod validate;

pub use client::{Client, ResolvedDataset};
pub use common::{ConcurrencyLimits, RemoteFile};
pub use config::{CliOverrides, Config, DownloadRequest, ResolvedConfig};
pub use iprox::Iprox;
pub use massive::Massive;
pub use pride::Pride;
pub use proteomexchange::{HostingRepository, PxDatasetInfo};
pub use repository::{Repo, Repository};
pub use update::{UpdateStatus, Version, check, current_version, latest_version, update_notice};
pub use validate::{fetch_checksums, validate_checksums, validate_file, validate_size};
