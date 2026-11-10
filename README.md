# pxdprowl

A Rust based utility for downloading datasets from [PRIDE Archive](https://www.ebi.ac.uk/pride).

# Installation
- Install pxdprowl via Cargo
- Run pxdprowl on your favourite dataset

# Usage
## Binary
## Library
pxdprowl can be used as a library. See

# Configuration
The binary reads an optional TOML config file: the path given with
`-c`/`--config`, or `./pxdprowl.toml` in the current directory when present.
Paths inside the file (like `output`) are resolved relative to the config
file's own directory, so a project config can be checked into version control
next to the data for reproducible downloads.

Each setting can be provided in up to four ways; per setting, the first
source that provides a value wins:

**CLI flag > environment variable > config file > built-in default**

| Setting | CLI flag | Environment variable | Config file key | Default |
|---|---|---|---|---|
| Output directory | `-o, --output DIR` | `PXDPROWL_OUTPUT` | `output` | `datasets` |
| Download concurrency | `--download-concurrency N` | `PXDPROWL_DOWNLOAD_CONCURRENCY` | `download-concurrency` | `min(2 * CPUs, 8)` |
| Validation concurrency (fixed; disables the adaptive limiter) | `--validate-concurrency N` | `PXDPROWL_VALIDATE_CONCURRENCY` | `validate-concurrency` | adaptive (AIMD) |
| Max retries per operation | `--max-retries N` | `PXDPROWL_MAX_RETRIES` | `max-retries` | `5` |

All numeric values must be positive integers. Unknown config keys and
malformed values are reported as errors.

## Example `pxdprowl.toml`
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

## Running from the config file
With no subcommand, `pxdprowl` runs the downloads listed in the config file
(datasets first, then `files`, then `urls`), continuing past individual
failures and summarizing them at the end:

```console
$ pxdprowl        # downloads everything listed in ./pxdprowl.toml
```

If there is nothing to download (no subcommand, and no `datasets`/`files`/
`urls` in the config file), `pxdprowl` prints its help with a hint instead of
failing.

When a subcommand is given, the config file's download lists are ignored (a
hint is printed); its settings (`output`, concurrency, retries, ...) still
apply. Subcommands remain useful for one-off actions:

```console
$ pxdprowl dataset PXD000001
$ pxdprowl files PXD000001
$ pxdprowl file name PXD000001 sample.raw
$ pxdprowl file url https://ftp.pride.ebi.ac.uk/.../sample.raw
```

## Library use
Library users skip the config file and env vars entirely by starting from
`pxdprowl::Config::default()` and adjusting fields:

```rust,ignore
let mut config = pxdprowl::Config::default();
config.output = "data".into();
config.download_concurrency = 4;
config.validate_concurrency = Some(8);
config.max_retries = 3;
```