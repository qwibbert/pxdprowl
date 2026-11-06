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
Three settings control how downloads and validations run. Each can be set in
four ways; per setting, the first source that provides a value wins:

**CLI flag > environment variable > config file > built-in default**

| Setting | CLI flag | Environment variable | Config file key | Default |
|---|---|---|---|---|
| Download concurrency | `--download-concurrency N` | `PRIDL_DOWNLOAD_CONCURRENCY` | `download-concurrency` | `min(2 * CPUs, 8)` |
| Validation concurrency (fixed; disables the adaptive limiter) | `--validate-concurrency N` | `PRIDL_VALIDATE_CONCURRENCY` | `validate-concurrency` | adaptive (AIMD) |
| Max retries per operation | `--max-retries N` | `PRIDL_MAX_RETRIES` | `max-retries` | `5` |

All values must be positive integers.

## Config file
The binary looks for a `pridl.toml` in the output directory (`-o`, default
`datasets/`) — the root of where your data is stored — so you can check it
into version control next to the data for reproducible downloads:

```toml
# datasets/pridl.toml
download-concurrency = 4
validate-concurrency = 8
max-retries = 3
```

Unknown keys and malformed values are reported as errors. Library users skip
the file and env vars entirely by constructing `pridl::Config` directly:

```rust,ignore
let config = pridl::Config {
    download_concurrency: 4,
    validate_concurrency: Some(8),
    max_retries: 3,
};
```