//! Binary entry point for `pride-rs`.
//!
//! All logic lives in the library crate ([`pride_rs`]); this file only parses
//! CLI arguments and dispatches to [`pride_rs::cli::run`].

use anyhow::Result;
use clap::Parser;
use pride_rs::cli::{Args, run};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    run(args).await
}
