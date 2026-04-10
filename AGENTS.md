# AGENTS.md - scrippiscrappa

A fast, concurrent website scraper written in Rust.

## Project Overview

- **Language**: Rust (Edition 2021)
- **Build System**: Cargo
- **Primary Dependencies**: clap, reqwest, tokio, scraper, url, anyhow, zip

## Build Commands

```bash
# Build debug
cargo build

# Build release
cargo build --release

# Build with locked dependencies (recommended for CI)
cargo build --release --locked

# Run
cargo run --release -- [args]

# Clean build artifacts
cargo clean
```

## Testing

This project currently has no test suite. If tests are added:

```bash
# Run all tests
cargo test

# Run a single test
cargo test test_name_here

# Run tests with output
cargo test -- --nocapture

# Run tests and watch for changes (requires cargo-watch)
cargo watch -x test
```

## Linting & Formatting

```bash
# Format code
cargo fmt

# Check formatting (CI uses this)
cargo fmt -- --check

# Clippy lints
cargo clippy --all-targets -- -D warnings

# All checks (format, clippy, tests)
cargo fmt && cargo clippy --all-targets && cargo test
```

## Code Style Guidelines

### General Conventions

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use Rust Edition 2021 features
- Target MSRV (Minimum Supported Rust Version): stable

### Naming Conventions

| Element | Convention | Example |
|---------|------------|---------|
| Functions | snake_case | `extract_links`, `fmt_size` |
| Variables | snake_case | `start_url`, `out_dir` |
| Types/Enums | PascalCase | `UrlType`, `CrawlState`, `Stats` |
| Constants | SCREAMING_SNAKE_CASE | `BOLD`, `GREEN`, `RESET` |
| Modules | snake_case | `mod example_module` |

### Imports

- Group imports by external crate, then standard library, then local
- Use `use` statements for frequently used items
- Prefer absolute paths with crate name for clarity in deeply nested code

```rust
// Ordering: std → external crates → local
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use scraper::{Html, Selector};
use tokio::fs;
```

### Error Handling

- Use `anyhow::Result<T>` for application-level errors
- Use `.context()` for adding context to errors
- Use `?` operator consistently
- Never silently ignore errors with `_`

```rust
// Good
fn normalize_url(input: &str) -> Result<String> {
    let s = format!("https://{}", input);
    Ok(url::Url::parse(&s).context("Invalid URL")?.to_string())
}

// Bad - ignores potential error
let host = url.host_str().unwrap_or("unknown");
```

### Async Code

- Use `tokio` for async runtime (already configured with `features = ["full"]`)
- Use `tokio::sync::Mutex` for async mutexes
- Use `std::sync::Mutex` only for non-async contexts
- Always handle `.await` results properly

```rust
// Good
let mut v = s.visited.lock().await;
if v.insert(link.clone()) {
    drop(v);
    enqueue(link, depth+1, md, &s, &tx, nl);
}
```

### Types

- Prefer explicit types for public API parameters
- Use atomic types for shared counters (`AtomicU64`, `AtomicUsize`)
- Use `Ordering::Relaxed` for simple counters (sufficient for statistics)
- Use `Arc<Mutex<T>>` or `Arc<Semaphore>` for shared state

### Formatting Rules

- 4 spaces for indentation
- Max line length: 100 characters (soft guideline)
- No trailing whitespace
- One blank line between function definitions
- Align struct literals and match arms when it aids readability

### Documentation

- Document public functions with doc comments
- Keep comments concise and meaningful
- Avoid obvious comments (e.g., `// Increment counter` for `counter += 1`)

## Project Structure

```
src/
  main.rs          # Single file containing all application code
Cargo.toml         # Dependencies and package config
.github/workflows/ # CI/CD pipeline
```

## CLI Design (clap)

- Use `#[derive(Parser)]` derive macro
- Use kebab-case for CLI arguments (`--no-limits`, `--archive-7z`)
- Provide sensible defaults
- Include help descriptions for all options

## Performance Considerations

- Use connection pooling via `reqwest::Client`
- Limit concurrency with `Semaphore`
- Track statistics with atomics to avoid locks
- Use `std::io::Write` for stderr output (faster than println!)

## Git Workflow

- Commits should pass `cargo fmt` and `cargo clippy`
- Use `--locked` flag for reproducible builds in CI
- Target: `main` branch for PRs

## Dependencies Management

- Use version constraints in Cargo.toml (e.g., `version = "4"`)
- Run `cargo update` to update dependencies
- Test with `--locked` before releasing
