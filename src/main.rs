//! Binary entry point for `pxdprowl`.
//!
//! All logic lives in the library crate ([`pxdprowl`]); this file only parses
//! CLI arguments and dispatches to [`pxdprowl::cli::run`].

use anyhow::Result;
use clap::Parser;
use pxdprowl::cli::{Args, run};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    run(args).await
}
