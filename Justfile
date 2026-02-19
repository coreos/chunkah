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

# Verify Cargo.lock and README version match Cargo.toml
versioncheck:
    #!/bin/bash
    set -euo pipefail
    cargo update chunkah --locked
    cargo_version=$(cargo metadata --no-deps --format-version=1 | jq -r '.packages[0].version')
    line=$(grep -E '^\s+https://github\.com/jlebon/chunkah/releases/download/v[0-9]+\.[0-9]+\.[0-9]+/Containerfile\.splitter$' README.md) \
        || { echo "Could not find Containerfile.splitter download URL in README.md"; exit 1; }
    readme_version=$(echo "${line}" | grep -oP 'download/v\K[0-9]+\.[0-9]+\.[0-9]+')
    if [[ "${cargo_version}" != "${readme_version}" ]]; then
        echo "Version mismatch: Cargo.toml has ${cargo_version}, README.md has ${readme_version}"
        exit 1
    fi

# Run all checks (shellcheck, unit tests, fmt, clippy, markdownlint, versioncheck)
checkall: shellcheck check fmt clippy markdownlint versioncheck

# Build chunkah container image (use --no-chunk to skip chunking for faster builds)
[arg("no_chunk", long="no-chunk", value="true")]
buildimg no_chunk="":
    #!/bin/bash
    set -euo pipefail
    buildah="${BUILDAH:-buildah}"
    args=(-t chunkah --layers=true {{ if no_chunk == "true" { "--build-arg=FINAL_FROM=rootfs" } else { "--skip-unused-stages=false" } }})
    # drop this once we can assume 1.44
    version=$(${buildah} version --json | jq -r '.version')
    if [[ $(echo -e "${version}\n1.44" | sort -V | head -n1) != "1.44" ]]; then
        args+=(-v "$PWD:/run/src" --security-opt=label=disable)
    fi
    echo ${buildah} build "${args[@]}" .
    ${buildah} build "${args[@]}" .
    rm -f out.ociarchive

# Run end-to-end tests with built chunkah image
test *ARGS:
    ./tests/e2e/run.sh {{ ARGS }}

# Profile chunkah with flamegraph (outputs flamegraph.svg)
profile *ARGS:
    just -f tools/perf/Justfile profile {{ ARGS }}

# Benchmark chunkah with hyperfine
benchmark *ARGS:
    just -f tools/perf/Justfile benchmark {{ ARGS }}

# Compare two container images for equivalence
diff +ARGS:
    #!/bin/bash
    set -euo pipefail
    img="localhost/chunkah-differ:latest"
    if ! podman image exists "${img}"; then
        podman build -t "${img}" tools/differ
    fi
    # Split args: first two are image names, rest are passed through
    args=({{ ARGS }})
    image1="${args[0]}"
    image2="${args[1]}"
    podman run --rm -v /var/tmp \
        --mount=type=image,src="${image1}",target=/image1 \
        --mount=type=image,src="${image2}",target=/image2 \
        "${img}" /image1 /image2 "${args[@]:2}"

# Split an existing image using the splitter Containerfile
split IMG *ARGS:
    #!/bin/bash
    set -euo pipefail
    shopt -s inherit_errexit
    buildah="${BUILDAH:-buildah}"
    args=(--skip-unused-stages=false --from "{{ IMG }}")
    args+=(--build-arg CHUNKAH=localhost/chunkah)
    args+=(--build-arg "CHUNKAH_CONFIG_STR=$(podman inspect {{ IMG }})")
    if [[ -n "{{ ARGS }}" ]]; then
        args+=(--build-arg "CHUNKAH_ARGS={{ ARGS }}")
    fi
    # drop this once we can assume 1.44
    version=$(${buildah} version --json | jq -r '.version')
    if [[ $(echo -e "${version}\n1.44" | sort -V | head -n1) != "1.44" ]]; then
        args+=(-v "$PWD:/run/src" --security-opt=label=disable)
    fi
    ${buildah} build "${args[@]}" Containerfile.splitter
    rm -f out.ociarchive

# Cut a release (use --no-push to prepare without pushing)
release *ARGS:
    ./tools/release.py {{ ARGS }}
