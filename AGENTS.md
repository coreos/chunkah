# AGENTS.md

This file provides guidance to coding agents when working with code in this
repository.

## Project Overview

chunkah is an OCI building tool that takes a flat rootfs and outputs a layered
OCI image with content-based layers. It "postprocesses" images so that layers
are created to maximize layer reuse rather than reflecting Dockerfile structure.

## Build and Test Commands

```bash
just build               # Build the project (dev)
just build release       # Build the project (release)
just check               # Run unit tests
just clippy              # Run clippy linter
just fmt                 # Check code formatting
just shellcheck          # Lint shell scripts
just markdownlint        # Lint markdown files
just checkall            # Run all checks (shellcheck, check, fmt, clippy, markdownlint)
just test                # Run all end-to-end tests (requires built container image)
just test fcos           # Run only the FCOS e2e test
just buildimg            # Build chunkah container image
just buildimg --no-chunk # Build chunkah container image without chunking (faster)
```

Run a single unit test:

```bash
cargo test test_name
```

After editing Rust code: `cargo fmt && just check && just clippy`

After editing shell scripts: `just shellcheck`

After editing markdown files: `just markdownlint`

## Architecture

### Core Pipeline

1. **scan** (`src/scan.rs`) - Walks the rootfs and builds a map of paths to
   their metadata
2. **components** (`src/components/`) - Determines which files belong to which
   components
3. **packing** (`src/packing.rs`) - Greedy clustering algorithm that merges
   components into layers
4. **ocibuilder** (`src/ocibuilder.rs`) - Creates OCI layers from components
5. **tar** (`src/tar.rs`) - Writes files to tar archives with proper metadata

### Component System

The `ComponentsRepo` trait (`src/components/mod.rs`) defines how different
package systems claim files:

- `rpm` - Claims files based on RPM database, groups by SRPM
- `xattr` - Claims files based on `user.component` extended attributes
- `bigfiles` - Claims individual large files (>1MB) as separate components

Repos have priorities; higher priority repos (lower values) win when claiming
paths. Unclaimed files go to `chunkah/unclaimed`.

### Commands

- `build` (`src/cmd_build.rs`) - Main command: scans rootfs, assigns
  components, builds OCI archive

## Code Guidelines

### Rust

#### Error Handling

- Uses `anyhow` for all error handling. Return `Result<T>` (i.e.
  `anyhow::Result<T>`) from all fallible functions.
- Never use `expect()` or `unwrap()` in production code. Use `Result` instead.
- Add `.context("...")` or `.with_context(|| format!("..."))` to errors.
  Context messages start with a **lowercase** gerund (e.g.,
  `context("opening file")`, `context("loading xattrs")`).
- Use `anyhow::bail!` for early-exit errors and `anyhow::ensure!` for
  assertions.
- In tests, `unwrap()` is fine.

#### Imports

Imports are organized in three groups separated by blank lines:

1. `std` imports
2. External crate imports (`anyhow`, `camino`, `clap`, etc.)
3. Internal imports (`crate::`, `super::`)

Each group is sorted alphabetically. `cargo fmt` handles this.

#### Patterns

- Builder pattern: methods consume and return `Self`
  (`pub fn compression(mut self, ...) -> Self`).
- Use `BTreeMap` for ordered maps (filesystem paths). `HashMap` for unordered.
- Use `camino::Utf8PathBuf`/`Utf8Path` for paths, not `std::path`.
- Use `cap-std-ext` for sandboxed filesystem access.

#### Function Ordering

- Public functions first, ordered by lifecycle: constructor, core operations,
  cleanup.
- Private helper functions after, in depth-first call order from the public
  functions that use them.
- `#[cfg(test)] mod tests` block at the bottom of each file.

#### Formatting

- Default `rustfmt` and `clippy` settings; all clippy warnings are errors
  (`-D warnings`).

#### Logging

Uses `tracing` crate. Log levels:

- **trace**: Per-item iteration details (e.g., each file scanned, each path
  claimed). Very verbose.
- **debug**: Flow of control, configuration values, state changes. Useful for
  diagnosing issues.
- **info**: Significant milestones (e.g., "scan complete", "build complete").
  Visible by default.
- **warn**: Unexpected but recoverable conditions (e.g., missing package
  metadata).
- **error**: Unused. We propagate errors up.

Syntax:

```rust
tracing::info!(files = files.len(), "scan complete");
tracing::debug!(path = %path, "canonicalized");  // %path uses Display
tracing::trace!(path = %path, "scanned file");
tracing::warn!(package = %name, "missing sourcerpm");
```

Guidelines:

- Messages are lowercase, no trailing punctuation.
- Use `%` prefix for `Display` formatting (cleaner output for paths).
- Use `?` prefix for `Debug` formatting (when needed).
- Default level is `info`; use `-v` for debug, `-vv` for trace.

### Bash

Script header (required):

```bash
#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit
```

- Always use curly braces for variables: `${foo}`, not `$foo`.
- Always double-quote variable expansions: `"${foo}"`.
- Run `just shellcheck` after editing.

### Markdown

Run `just markdownlint` after editing.

### Tests

Unit tests live in `#[cfg(test)] mod tests` blocks in each `.rs` file. They
use `tempfile::tempdir()` for filesystem tests and fixtures from
`tests/fixtures/`.

E2E tests are shell scripts in `tests/e2e/` named `test-<name>.sh`. They
require a built container image (`just buildimg`) and use `podman`, `buildah`,
`skopeo`, and `jq`. Run specific e2e tests with `just test <name>` (e.g.,
`just test fcos` runs `test-fcos.sh`).
