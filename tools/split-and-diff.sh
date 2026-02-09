#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit

# Verify an image split through chunkah has identical data/metadata except for
# mtimes by comparing rootfs tarballs with diffoscope.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS] <source-image> <output-file>

Split an image through chunkah and compare the result with diffoscope.

Options:
    --raw              Compare raw layer tarballs without flattening (requires
                       single-layer images)
    --no-filter-mtime  Don't filter out mtime differences from output
    --no-cleanup       Don't remove images from containers-storage on exit
    -h, --help         Show this help message

Arguments:
    source-image    Source image to split (e.g. docker://quay.io/fedora/fedora-minimal)
    output-file     Path to write diffoscope output (filtered for mtime)

Environment:
    CHUNKAH_IMG     Container image to use for splitting
                    (default: quay.io/jlebon/chunkah:latest)

Exit codes:
    0   No differences found (output file is empty)
    1   Differences found (output file contains diff)

Examples:
    $(basename "$0") docker://quay.io/fedora/fedora-minimal /tmp/diff.txt
    $(basename "$0") --raw oci-archive:image.ociarchive /tmp/diff.txt
    CHUNKAH_IMG=localhost/chunkah:latest $(basename "$0") docker://... /tmp/out.txt
EOF
}

# Parse options
raw_mode=false
filter_mtime=true
cleanup_images=true
while [[ $# -gt 0 ]]; do
    case "${1}" in
        --raw)
            raw_mode=true
            shift
            ;;
        --no-filter-mtime)
            filter_mtime=false
            shift
            ;;
        --no-cleanup)
            cleanup_images=false
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "Error: Unknown option: ${1}" >&2
            echo "Try '$(basename "$0") --help' for more information." >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

# Validate arguments
if [[ $# -lt 2 ]]; then
    echo "Error: Missing required arguments" >&2
    echo "Try '$(basename "$0") --help' for more information." >&2
    exit 1
fi

source_image="${1}"
output_file="${2}"

# Check required tools
for tool in skopeo podman buildah diffoscope jq; do
    if ! command -v "${tool}" &> /dev/null; then
        echo "Error: Required tool '${tool}' not found" >&2
        exit 1
    fi
done

# Generate unique run ID for image names to allow concurrent runs
run_id="chunkah-diff-$$-$(date +%s)"

# Setup cleanup trap with safe handling of unset variables
tmpdir=""
original_image=""
split_image=""
# shellcheck disable=SC2329  # invoked via trap
cleanup() {
    if [[ -n "${tmpdir}" ]]; then
        chmod -R u+w "${tmpdir}"
        rm -rf "${tmpdir}"
    fi
    if [[ "${cleanup_images}" == "true" ]]; then
        if [[ -n "${original_image}" ]]; then podman rmi -f "${original_image}" 2>/dev/null || true; fi
        if [[ -n "${split_image}" ]]; then podman rmi -f "${split_image}" 2>/dev/null || true; fi
    fi
}
trap cleanup EXIT

tmpdir=$(mktemp -d)

# Extract rootfs tarball from an OCI archive by flattening all layers
extract_rootfs_tarball() {
    local ociarchive="${1}"
    local output_tar="${2}"
    local workdir
    workdir=$(mktemp -d)

    mkdir -p "${workdir}/oci" "${workdir}/rootfs"
    tar -xf "${ociarchive}" -C "${workdir}/oci"

    local manifest_digest
    manifest_digest=$(jq -r '.manifests[0].digest | split(":")[1]' "${workdir}/oci/index.json")

    # Extract each layer in order (auto-detect compression)
    jq -r '.layers[].digest | split(":")[1]' "${workdir}/oci/blobs/sha256/${manifest_digest}" | \
    while read -r layer_digest; do
        local layer_blob="${workdir}/oci/blobs/sha256/${layer_digest}"
        # Use file magic to detect compression, fallback to gzip
        case "$(file -b "${layer_blob}")" in
            *gzip*)       tar -xzf "${layer_blob}" -C "${workdir}/rootfs" ;;
            *zstd*)       tar -I zstd -xf "${layer_blob}" -C "${workdir}/rootfs" ;;
            *POSIX\ tar*) tar -xf "${layer_blob}" -C "${workdir}/rootfs" ;;
            *)            tar -xzf "${layer_blob}" -C "${workdir}/rootfs" ;;
        esac
    done

    # Fix files with permissions 000 so they can be read for comparison
    find "${workdir}/rootfs" -type f -perm 000 -exec chmod 644 {} +

    # Create deterministic tarball (sorted entries)
    tar -cf "${output_tar}" -C "${workdir}/rootfs" --sort=name .
    chmod -R u+w "${workdir}"
    rm -rf "${workdir}"
}

# Get raw layer tarball from an OCI archive (for --raw mode)
get_raw_layer_tarball() {
    local ociarchive="${1}"
    local output_tar="${2}"
    local workdir
    workdir=$(mktemp -d)

    tar -xf "${ociarchive}" -C "${workdir}"

    local manifest_digest
    manifest_digest=$(jq -r '.manifests[0].digest | split(":")[1]' "${workdir}/index.json")

    # Get the single layer (--raw mode expects single-layer images)
    local layer_count
    layer_count=$(jq '.layers | length' "${workdir}/blobs/sha256/${manifest_digest}")
    if [[ "${layer_count}" -ne 1 ]]; then
        echo "Error: --raw mode requires single-layer images, found ${layer_count} layers" >&2
        rm -rf "${workdir}"
        return 1
    fi

    local layer_digest
    layer_digest=$(jq -r '.layers[0].digest | split(":")[1]' "${workdir}/blobs/sha256/${manifest_digest}")

    # Copy the raw layer blob (may be compressed)
    cp "${workdir}/blobs/sha256/${layer_digest}" "${output_tar}"
    rm -rf "${workdir}"
}

# Prepare original image (use localhost/ prefix for consistent naming)
echo "Preparing original image..."
original_image="localhost/${run_id}-original:latest"
skopeo copy "${source_image}" "containers-storage:${original_image}"

# Flatten if multi-layer
layer_count=$(podman inspect --format '{{len .RootFS.Layers}}' "${original_image}")
if [[ "${layer_count}" -gt 1 ]]; then
    echo "Flattening multi-layer image (${layer_count} layers)..."
    squashed_image="localhost/${run_id}-original-squashed:latest"
    buildah build --squash -t "${squashed_image}" -f - <<< "FROM containers-storage:${original_image}"
    podman rmi "${original_image}"
    podman tag "${squashed_image}" "${original_image}"
    podman rmi "${squashed_image}"
fi

# Export to ociarchive for comparison
skopeo copy "containers-storage:${original_image}" "oci-archive:${tmpdir}/original.ociarchive"

# Split image via Containerfile.splitter
echo "Splitting image through chunkah..."
config_str=$(podman inspect "${original_image}")

# Check buildah version for compatibility (< 1.44 needs extra args)
buildah_args=()
buildah_version=$(buildah version --json | jq -r '.version')
min_version=$(echo -e "${buildah_version}\n1.44" | sort -V | head -n1)
if [[ "${min_version}" != "1.44" ]]; then
    buildah_args+=(-v "${tmpdir}:/run/src" --security-opt=label=disable)
fi

# Support custom chunkah image via CHUNKAH_IMG env var
chunkah_img="${CHUNKAH_IMG:-quay.io/jlebon/chunkah:latest}"

pushd "${tmpdir}" > /dev/null
buildah build --skip-unused-stages=false \
    "${buildah_args[@]}" \
    --from "containers-storage:${original_image}" \
    --build-arg "CHUNKAH=${chunkah_img}" \
    --build-arg CHUNKAH_ARGS="--max-layers=1" \
    --build-arg "CHUNKAH_CONFIG_STR=${config_str}" \
    "${REPO_ROOT}/Containerfile.splitter"
popd > /dev/null

split_image=$(podman load -i "${tmpdir}/out.ociarchive" | awk '{print $NF}' | sed 's/^sha256://')
skopeo copy "containers-storage:${split_image}" "oci-archive:${tmpdir}/split.ociarchive"

# Extract and compare
echo "Comparing images with diffoscope..."
if [[ "${raw_mode}" == "true" ]]; then
    # Raw mode: compare unextracted layer tarballs directly
    get_raw_layer_tarball "${tmpdir}/original.ociarchive" "${tmpdir}/original-layer.tar"
    get_raw_layer_tarball "${tmpdir}/split.ociarchive" "${tmpdir}/split-layer.tar"
    original_tar="${tmpdir}/original-layer.tar"
    split_tar="${tmpdir}/split-layer.tar"
else
    # Default: flatten layers to rootfs tarballs
    extract_rootfs_tarball "${tmpdir}/original.ociarchive" "${tmpdir}/original-rootfs.tar"
    extract_rootfs_tarball "${tmpdir}/split.ociarchive" "${tmpdir}/split-rootfs.tar"
    original_tar="${tmpdir}/original-rootfs.tar"
    split_tar="${tmpdir}/split-rootfs.tar"
fi

diffoscope "${original_tar}" "${split_tar}" \
    > "${tmpdir}/diffoscope-raw.txt" 2>&1 || true

if [[ "${filter_mtime}" == "true" ]]; then
    # Filter out mtime-related lines
    grep -v -E '(mtime|Mtime|modification time)' "${tmpdir}/diffoscope-raw.txt" \
        > "${output_file}" || true
else
    cp "${tmpdir}/diffoscope-raw.txt" "${output_file}"
fi

# Exit 0 if no differences (empty output), 1 if differences found
if [[ -s "${output_file}" ]]; then
    echo "Differences found (see ${output_file})"
    exit 1
fi

echo "No differences found"
exit 0
