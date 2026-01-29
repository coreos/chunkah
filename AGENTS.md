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
just markdownlint        # Lint markdown files
just checkall            # Run all checks (shellcheck, check, fmt, clippy, markdownlint)
just test                # Run all end-to-end tests (requires built container image)
just test fcos           # Run only the FCOS e2e test
just buildimg            # Build chunkah container image
just buildimg --no-chunk # Build chunkah container image without chunking (faster)
```

Run a single test:

```bash
cargo test test_name
```

## Architecture

### Core Pipeline

1. **scan** (`src/scan.rs`) - Walks the rootfs and builds a map of paths to types
2. **components** (`src/components/`) - Determines which files belong to which
   components
3. **ocibuilder** (`src/ocibuilder.rs`) - Creates OCI layers from components
4. **tar** (`src/tar.rs`) - Writes files to tar archives with proper metadata

### Component System

The `ComponentsRepo` trait (`src/components/mod.rs`) defines how different
package systems claim files:

- `rpm` - Claims files based on RPM database, groups by SRPM
- `xattr` - Claims files based on `user.component` extended attributes

Repos have priorities; higher priority repos (lower values) win when claiming
paths. Unclaimed files go to `chunkah/unclaimed`.

### Commands

- `build` (`src/cmd_build.rs`) - Main command: scans rootfs, assigns
  components, builds OCI archive

## Code Guidelines

### Rust

- Never use `expect()` or `unwrap()` for error handling. Use `Result` instead.
- Add `context()` or `with_context()` to errors (lowercase first word:
  `context("opening file")`).
- After editing: `cargo fmt && just check && just clippy`

### Bash

Script header:

```bash
#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit
```

Always use curly braces: `${foo}`, not `$foo`. Run `just shellcheck` after
editing.

### Markdown

Run `just markdownlint` after editing.
