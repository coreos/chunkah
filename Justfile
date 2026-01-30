# Build the project
build profile="dev":
    cargo build --profile {{ profile }}

# Check code formatting
fmt:
    cargo fmt --check

# Run unit tests
check:
    cargo test

# Run clippy linter
clippy:
    cargo clippy -- -D warnings

# Lint shell scripts
shellcheck:
    shellcheck --external-sources --enable=all $(git ls-files '*.sh')

# Lint markdown files
markdownlint:
    markdownlint $(git ls-files '*.md')

# Verify Cargo.lock is in sync with Cargo.toml
lockcheck:
    cargo update chunkah --locked

# Run all checks (shellcheck, unit tests, fmt, clippy, markdownlint)
checkall: shellcheck check fmt clippy markdownlint

# Build chunkah container image (use --no-chunk to skip chunking for faster builds)
[arg("no_chunk", long="no-chunk", value="true")]
buildimg no_chunk="":
    #!/bin/bash
    set -euo pipefail
    buildah="${BUILDAH:-buildah}"
    args=(-t chunkah --layers=true {{ if no_chunk == "true" { "--build-arg=FINAL_FROM=rootfs" } else { "--skip-unused-stages=false" } }})
    # drop this once we can assume 1.43
    version=$(${buildah} version --json | jq -r '.version')
    if [[ $(echo -e "${version}\n1.43" | sort -V | head -n1) != "1.43" ]]; then
        args+=(-v "$PWD:/run/src" --security-opt=label=disable)
    fi
    echo ${buildah} build "${args[@]}" .
    ${buildah} build "${args[@]}" .

# Run end-to-end tests with built chunkah image
test *ARGS:
    ./tests/e2e/run.sh {{ ARGS }}

# Profile chunkah with flamegraph (outputs flamegraph.svg)
profile *ARGS:
    just -f tools/perf/Justfile profile {{ ARGS }}

# Benchmark chunkah with hyperfine
benchmark *ARGS:
    just -f tools/perf/Justfile benchmark {{ ARGS }}

# Cut a release (use --no-push to prepare without pushing)
release *ARGS:
    ./tools/release.py {{ ARGS }}
