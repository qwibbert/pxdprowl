//! `pride-rs` — download datasets and files from the
//! [PRIDE](https://www.ebi.ac.uk/pride) proteomics archive.
//!
//! The crate is split into a library (this module tree) and a thin binary
//! entry point (`src/main.rs`). All logic lives here so it is testable and
//! reusable.

// Native `async fn` in traits is the recommended style (stable since 1.75);
// the `Send`-bound caveat does not affect this crate because every spawned
// future is `Send` at the concrete impl site.
#![allow(async_fn_in_trait)]

pub mod cli;
pub mod common;
pub mod pride;
pub mod repository;
pub mod validate;

pub use pride::Pride;
pub use repository::Repository;
pub use validate::validate_checksums;
