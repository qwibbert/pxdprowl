//! Integration test: `pxdprowl PXD000001` (a bare dataset accession, no
//! subcommand) prints the command hint and succeeds instead of failing with
//! clap's "unrecognized subcommand" error. (Only one test per binary may call
//! `pxdprowl::cli::run`, because it installs the global logger.)

use clap::Parser;
use pxdprowl::cli::{Args, run};

#[tokio::test]
async fn bare_accession_prints_hint_and_succeeds() {
    // `--no-update-check` keeps the run offline.
    let args = Args::try_parse_from(["pxdprowl", "--no-update-check", "PXD000001"]).unwrap();
    assert_eq!(args.accession.as_deref(), Some("PXD000001"));
    assert!(args.command.is_none());
    run(args).await.unwrap();
}
