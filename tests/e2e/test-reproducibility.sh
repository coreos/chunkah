#!/bin/bash
# Test that chunked images are reproducible.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

SOURCE_IMAGE="quay.io/fedora/fedora-minimal:latest"

cleanup() {
    cleanup_images "${SOURCE_IMAGE}"
}
trap cleanup EXIT

podman pull "${SOURCE_IMAGE}"
CHUNKAH_CONFIG_STR=$(podman inspect "${SOURCE_IMAGE}")

assert_archives_identical() {
    local sum1 sum2
    sum1=$(sha256sum out1.ociarchive | cut -d' ' -f1)
    sum2=$(sha256sum out2.ociarchive | cut -d' ' -f1)
    if [[ "${sum1}" != "${sum2}" ]]; then
        echo "ERROR: OCI archives differ between builds"
        echo "Build 1: ${sum1}"
        echo "Build 2: ${sum2}"
        if command -v diffoscope &>/dev/null; then
            diffoscope out1.ociarchive out2.ociarchive || true
        fi
        exit 1
    fi
}

# Test 1: reproducible with explicit SOURCE_DATE_EPOCH
for i in 1 2; do
    podman run --rm --mount=type=image,src="${SOURCE_IMAGE}",target=/chunkah \
        -e CHUNKAH_CONFIG_STR="${CHUNKAH_CONFIG_STR}" \
        -e SOURCE_DATE_EPOCH=1700000000 \
            "${CHUNKAH_IMG:?}" build -v > "out${i}.ociarchive"
done
assert_archives_identical

# Test 2: reproducible without SOURCE_DATE_EPOCH (uses image's Created timestamp)
for i in 1 2; do
    podman run --rm --mount=type=image,src="${SOURCE_IMAGE}",target=/chunkah \
        -e CHUNKAH_CONFIG_STR="${CHUNKAH_CONFIG_STR}" \
            "${CHUNKAH_IMG:?}" build -v > "out${i}.ociarchive"
done
assert_archives_identical
