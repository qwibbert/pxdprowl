# PRIDL

A Rust based utility for downloading datasets from [PRIDE Archive](https://www.ebi.ac.uk/pride).

# Installation
- Install PRIDE-rs via Cargo
- Run PRIDE-rs on your favourite dataset

# Usage
## Binary
## Library
PRIDL can be used as a library. See

# Configuration
The binary reads an optional TOML config file: the path given with
`-c`/`--config`, or `./pridl.toml` in the current directory when present.
Paths inside the file (like `output`) are resolved relative to the config
file's own directory, so a project config can be checked into version control
next to the data for reproducible downloads.

Each setting can be provided in up to four ways; per setting, the first
source that provides a value wins:

**CLI flag > environment variable > config file > built-in default**

| Setting | CLI flag | Environment variable | Config file key | Default |
|---|---|---|---|---|
| Output directory | `-o, --output DIR` | `PRIDL_OUTPUT` | `output` | `datasets` |
| Download concurrency | `--download-concurrency N` | `PRIDL_DOWNLOAD_CONCURRENCY` | `download-concurrency` | `min(2 * CPUs, 8)` |
| Validation concurrency (fixed; disables the adaptive limiter) | `--validate-concurrency N` | `PRIDL_VALIDATE_CONCURRENCY` | `validate-concurrency` | adaptive (AIMD) |
| Max retries per operation | `--max-retries N` | `PRIDL_MAX_RETRIES` | `max-retries` | `5` |

All numeric values must be positive integers. Unknown config keys and
malformed values are reported as errors.

## Example `pridl.toml`
```toml
# Where downloaded files go (relative to this file).
output = "data"

download-concurrency = 4
validate-concurrency = 8
max-retries = 3

# What to download when running `pridl` with no subcommand.
datasets = ["PXD000001", "PXD000002"]

[[files]]
accession = "PXD000001"
name = "sample.raw"

urls = ["https://ftp.pride.ebi.ac.uk/pride/data/archive/2024/01/PXD000003/extra.raw"]
```

## Running from the config file
With no subcommand, `pridl` runs the downloads listed in the config file
(datasets first, then `files`, then `urls`), continuing past individual
failures and summarizing them at the end:

```console
$ pridl        # downloads everything listed in ./pridl.toml
```

If there is nothing to download (no subcommand, and no `datasets`/`files`/
`urls` in the config file), `pridl` prints its help with a hint instead of
failing.

When a subcommand is given, the config file's download lists are ignored (a
hint is printed); its settings (`output`, concurrency, retries, ...) still
apply. Subcommands remain useful for one-off actions:

```console
$ pridl dataset PXD000001
$ pridl files PXD000001
$ pridl file name PXD000001 sample.raw
$ pridl file url https://ftp.pride.ebi.ac.uk/.../sample.raw
```

## Library use
Library users skip the config file and env vars entirely by constructing
`pridl::Config` directly:

```rust,ignore
let config = pridl::Config {
    output: "data".into(),
    download_concurrency: 4,
    validate_concurrency: Some(8),
    max_retries: 3,
};
```