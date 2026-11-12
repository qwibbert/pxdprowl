# pxdprowl

[![Crates.io](https://img.shields.io/crates/v/pxdprowl)](https://crates.io/crates/pxdprowl)
[![docs.rs](https://img.shields.io/docsrs/pxdprowl)](https://docs.rs/pxdprowl)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/qwibbert/pxdprowl/blob/master/LICENSE)

Download proteomics datasets by **PXD accession** from the
[ProteomeXchange](https://www.proteomexchange.org) repositories — **PRIDE**,
**MassIVE** and **iProX** — with concurrent downloads, resume support, and
integrity validation.

## Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
  - [Subcommands](#subcommands)
  - [Batch downloads from a config file](#batch-downloads-from-a-config-file)
- [Configuration](#configuration)
- [Dataset resolution and supported repositories](#dataset-resolution-and-supported-repositories)
- [Library use](#library-use)
- [License](#license)

## Features

- Resolves any PXD accession to its actual hosting repository via
  ProteomeXchange (PROXI API) and downloads from **PRIDE**, **MassIVE** or
  **iProX**
- Only RAW files per dataset; each file's relative path is preserved under
  `output/{PXD}/`
- Concurrent downloads with global concurrency caps, resume from `.part`
  files, and per-file retries with backoff
- Integrity validation: MD5 checksums for PRIDE; size validation for MassIVE
  and iProX (they publish no per-file checksums)
- Rate-limit aware: serializes MassIVE downloads and honors `Retry-After` on
  HTTP 429
- Settings via CLI flags, environment variables, or an optional TOML config
  file; config-file batches run concurrently and summarize failures at the end
- Usable as a Rust library

## Installation

```console
$ cargo install pxdprowl
```

Or build from source (requires a recent stable Rust toolchain, edition 2024):

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
```

Downloaded files land in `output/{PXD}/...` (default output directory:
`datasets`). Global flags — `-c/--config PATH`, `-o/--output DIR`, `-v`,
`--download-concurrency N`, `--validate-concurrency N`, `--max-retries N` —
are described under [Configuration](#configuration).

### Batch downloads from a config file

With no subcommand, `pxdprowl` runs the downloads listed in the config file
(the path given with `-c`, or `./pxdprowl.toml` in the current directory) —
datasets first, then `files`, then `urls` — **concurrently**, so a small
dataset doesn't delay the others. Individual failures don't stop the batch;
they are summarized at the end:

```console
$ pxdprowl        # downloads everything listed in ./pxdprowl.toml
```

If there is nothing to download (no subcommand, and no `datasets`/`files`/
`urls` in the config file), `pxdprowl` prints its help with a hint instead of
failing. When a subcommand is given, the config file's download lists are
ignored (a hint is printed); its settings (`output`, concurrency, retries,
...) still apply.

## Configuration

Each setting can be provided in up to four ways; per setting, the first source
that provides a value wins:

**CLI flag > environment variable > config file > built-in default**

| Setting | CLI flag | Environment variable | Config file key | Default |
|---|---|---|---|---|
| Output directory | `-o, --output DIR` | `PXDPROWL_OUTPUT` | `output` | `datasets` |
| Download concurrency | `--download-concurrency N` | `PXDPROWL_DOWNLOAD_CONCURRENCY` | `download-concurrency` | `min(2 * CPUs, 8)` |
| Validation concurrency (fixed; disables the adaptive limiter) | `--validate-concurrency N` | `PXDPROWL_VALIDATE_CONCURRENCY` | `validate-concurrency` | adaptive (AIMD) |
| Max retries per operation | `--max-retries N` | `PXDPROWL_MAX_RETRIES` | `max-retries` | `5` |

All numeric values must be positive integers. Unknown config keys and
malformed values are reported as errors. Paths inside the config file (like
`output`) are resolved relative to the config file's own directory, so a
project config can be checked into version control next to the data for
reproducible downloads.

Example `pxdprowl.toml`:

```toml
# Where downloaded files go (relative to this file).
output = "data"

download-concurrency = 4
validate-concurrency = 8
max-retries = 3

# What to download when running `pxdprowl` with no subcommand.
datasets = ["PXD000001", "PXD000002"]

[[files]]
accession = "PXD000001"
name = "sample.raw"

urls = ["https://ftp.pride.ebi.ac.uk/pride/data/archive/2024/01/PXD000003/extra.raw"]
```

## Dataset resolution and supported repositories

PXD accessions are issued by
[ProteomeXchange](https://www.proteomexchange.org), which spans several
repositories (PRIDE, MassIVE, iProX, jPOST, Panorama Public, PeptideAtlas) — a
PXD id does not imply the data is in PRIDE. Before downloading, `pxdprowl`
asks ProteomeXchange (PROXI API) which repository hosts the dataset and
delegates to that repository's downloader.

Supported so far: **PRIDE**, **MassIVE** and **iProX**. A dataset hosted on any
other repository (e.g. a jPOST one) fails with a clear message naming the
repository.

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
  rate-limits parallel requests — pxdprowl therefore serializes MassIVE
  downloads and honors `Retry-After` on HTTP 429. MassIVE responses are
  chunked (no `Content-Length`), so progress-bar totals come from the file
  index's reported sizes.
- **iProX**: file URLs recorded as `download.iprox.org` are rewritten to
  `https://download.iprox.cn` (same server; the `.org` host's certificate is
  only valid for `*.iprox.cn`, so HTTPS to it fails verification).

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
```

The crate is organized into modules: `cli` (argument parsing and dispatch),
`config` (settings resolution), `proteomexchange` (PXD → repository
resolution), `repository` (the `Repository` trait, the `Repo` dispatcher and
the shared download engine), the per-repository implementations `pride`,
`massive` and `iprox`, and `validate` (checksum/size validation).

## License

MIT — see [LICENSE](https://github.com/qwibbert/pxdprowl/blob/master/LICENSE).
