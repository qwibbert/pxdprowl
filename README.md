# pxdprowl

[![Crates.io](https://img.shields.io/crates/v/pxdprowl)](https://crates.io/crates/pxdprowl)
[![docs.rs](https://img.shields.io/docsrs/pxdprowl)](https://docs.rs/pxdprowl)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/qwibbert/pxdprowl/blob/master/LICENSE)

Download proteomics datasets by **PXD accession** (e.g. `PXD000001`) from the
[ProteomeXchange](https://www.proteomexchange.org) repositories **PRIDE**,
**MassIVE** and **iProX**, with concurrent downloads, resume support and
integrity validation.

## Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
  - [Subcommands](#subcommands)
  - [Batch downloads from a config file](#batch-downloads-from-a-config-file)
- [Configuration](#configuration)
- [Dataset resolution and supported repositories](#dataset-resolution-and-supported-repositories)
- [Download resuming](#download-resuming)
- [Library use](#library-use)
- [License](#license)

## Features

- Download any **PRIDE**, **MassIVE** or **iProX** dataset — RAW files only
  (the instrument's raw output, e.g. `.raw`/`.d`/`.wiff`, not processed
  formats like mzML)
- Concurrent downloads, resume support after failure, and per-file retries with backoff
- Integrity validation: MD5 checksums for PRIDE and size validation for MassIVE
  and iProX (they publish no per-file checksums)
- Rate-limit aware: serializes MassIVE downloads and honors `Retry-After` on
  HTTP 429
- Settings via CLI flags, environment variables, or an optional TOML config
  file; config-file batches run concurrently and summarize failures at the end
- Built-in update check against crates.io: every command checks once a day in
  the background and prints a notice when a newer release is available (plus
  the explicit `pxdprowl update` subcommand); disable with `--no-update-check`
- Usable as a Rust library

## Installation

PXDProwl is a Rust binary, so install the Rust toolchain first via
[rustup](https://rustup.rs/).

### Install the binary

```console
$ cargo install pxdprowl
```

Re-running `cargo install pxdprowl` upgrades to the latest released version.
PXDProwl also checks crates.io for newer releases on its own — once a day
after any command, and any time via `pxdprowl update` — and prints a notice
when an update is available (disable with `--no-update-check`, see
[Configuration](#configuration)).

### Build from source

Requires a recent stable Rust toolchain (edition 2024):

```console
$ cargo build --release    # binary at target/release/pxdprowl
```

## Usage

### Subcommands

```console
$ pxdprowl dataset PXD000001                # download all RAW files of a dataset
$ pxdprowl files PXD000001                  # list the RAW files of a dataset
$ pxdprowl file name PXD000001 sample.raw   # download one file by name
$ pxdprowl file url https://.../sample.raw  # download one file by URL
$ pxdprowl update                           # check crates.io for a newer release
```

Downloaded files land in `datasets/{PXD}/...` (the default output directory can be configured, see [Configuration](#configuration)).

Input is validated before anything is requested or written: accessions must be ProteomeXchange IDs (`PXD` followed by digits, e.g. `PXD000001`), `file url` accepts HTTPS URLs (plain HTTP only for loopback hosts), and file paths reported by a repository that would escape the output directory (`..`, absolute paths) are rejected.

### Batch downloads from a config file

With no subcommand, `pxdprowl` will try to find a config file (`pxdprowl.toml`) in the current directory. The structure of this config file is described in the [Configuration](#configuration) section. If it doesn't find a config file, PXDProwl will show a help message with a hint.

```console
$ pxdprowl        # downloads everything listed in ./pxdprowl.toml
```

## Configuration

PXDProwl has 3 ways of adjusting its settings (in order of importance): CLI flags, environment variables or a config file. The available settings are listed below. When both a [subcommand](#subcommands) and a config file are present, the program will ignore the downloads listed in the config file.

| Setting | CLI flag | Environment variable | Config file key | Default |
|---|---|---|---|---|
| Output directory | `-o, --output DIR` | `PXDPROWL_OUTPUT` | `output` | `datasets` |
| Download concurrency | `--download-concurrency N` | `PXDPROWL_DOWNLOAD_CONCURRENCY` | `download-concurrency` | `min(2 * CPUs, 8)` |
| Validation concurrency (fixed; disables the adaptive limiter) | `--validate-concurrency N` | `PXDPROWL_VALIDATE_CONCURRENCY` | `validate-concurrency` | adaptive |
| Max retries per operation | `--max-retries N` | `PXDPROWL_MAX_RETRIES` | `max-retries` | `5` |
| Update check against crates.io | `--no-update-check` | `PXDPROWL_UPDATE_CHECK` | `update-check` | enabled |
| Config file path | `-c, --config PATH` | — | — | `./pxdprowl.toml` |
| Verbosity | `-v` / `-vv` / `-vvv` | `RUST_LOG` | — | warnings only |

`-v` shows info logs, `-vv` debug, `-vvv` trace; `RUST_LOG` (e.g. `RUST_LOG=debug`) does the same without flags.

Example `pxdprowl.toml`:

```toml
# Where downloaded files go (relative to this file).
output = "datasets" # default: datasets

download-concurrency = 4 # default: inferred from CPU count, max 8
validate-concurrency = 8 # default: adaptive (auto-tuned)
max-retries = 3 # default: 5
update-check = true # default: true; check crates.io for newer releases once a day

# What to download when running `pxdprowl` with no subcommand.
# List any combination of the three options below.
datasets = ["PXD000001", "PXD000002"]

[[files]]
accession = "PXD000001"
name = "sample.raw"

urls = ["https://ftp.pride.ebi.ac.uk/pride/data/archive/2024/01/PXD000003/extra.raw"]
```

## Dataset resolution and supported repositories

PXD accessions are issued by
[ProteomeXchange](https://www.proteomexchange.org), which spans several
repositories (PRIDE, MassIVE, iProX, jPOST, Panorama Public, PeptideAtlas). Before downloading, `pxdprowl`
asks ProteomeXchange (PROXI API) which repository hosts the dataset and
delegates to that repository's downloader.

Supported so far: **PRIDE**, **MassIVE** and **iProX**. A dataset hosted on any
other repository (e.g. a jPOST one) will fail to download.

For every dataset, only the RAW files are listed/downloaded (PRIDE `RAW`
category; MassIVE `raw/` collection; iProX "Associated raw file URI" entries),
and each file's relative path is preserved under `output/{PXD}/` (e.g.
`output/PXD004584/raw/Replicate 1/a.raw`).

Integrity: PRIDE publishes an MD5 checksum TSV per dataset, so PRIDE downloads
are MD5-validated. MassIVE and iProX publish no per-file checksums, so their
downloads are validated by size (the listing's reported size / HTTP
`Content-Length`) instead.

Repository-specific notes:

- **MassIVE**: files are served over HTTPS via the ProteoSAFe
  `DownloadResultFile` endpoint (the FTPS server is not used), which
  rate-limits parallel requests. PXDProwl therefore serializes MassIVE
  downloads and honors `Retry-After` on HTTP 429. MassIVE responses are
  chunked (no `Content-Length`), so progress-bar totals come from the file
  index's reported sizes.
- **iProX**: file URLs recorded as `download.iprox.org` are rewritten to
  `https://download.iprox.cn` (same server; the `.org` host's certificate is
  only valid for `*.iprox.cn`, so HTTPS to it fails verification).

## Download resuming

Each file is first downloaded to a `.part` file next to its destination and renamed when complete. If a download is interrupted (network drop, Ctrl-C, ...), just re-run the same command: PXDProwl detects the `.part` file, asks the server for the remaining bytes with an HTTP `Range` request, and continues where it left off. Files that already exist in full are skipped, so re-running a finished dataset is a no-op.

```console
$ pxdprowl dataset PXD000001   # gets interrupted...
$ pxdprowl dataset PXD000001   # ...resumes automatically from the .part file
```

Tip: run with `-vv` to see resume/skip decisions in the logs.

## Library use

`pxdprowl` can be used as a library. Library users skip the config file and
env vars entirely by starting from `pxdprowl::Config::default()` and adjusting
fields:

```rust,ignore
let mut config = pxdprowl::Config::default();
config.output = "data".into();
config.download_concurrency = 4;
config.validate_concurrency = Some(8);
config.max_retries = 3;
config.update_check = false;
```

The crate is organized into modules: `cli` (argument parsing and dispatch),
`config` (settings resolution), `proteomexchange` (PXD → repository
resolution), `repository` (the `Repository` trait, the `Repo` dispatcher and
the shared download engine), the per-repository implementations `pride`,
`massive` and `iprox`, `update` (crates.io update checks), and `validate`
(checksum/size validation).

## License

MIT — see [LICENSE](https://github.com/qwibbert/pxdprowl/blob/master/LICENSE).
