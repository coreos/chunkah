#!/bin/bash
# Test that chunked images are reproducible when built with SOURCE_DATE_EPOCH.
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

# run chunkah twice on the same rootfs with a fixed SOURCE_DATE_EPOCH
for i in 1 2; do
    podman run --rm --mount=type=image,src="${SOURCE_IMAGE}",target=/chunkah \
        -e CHUNKAH_CONFIG_STR="${CHUNKAH_CONFIG_STR}" \
        -e SOURCE_DATE_EPOCH=1700000000 \
            "${CHUNKAH_IMG:?}" build > "out${i}.ociarchive"
done

# the two OCI archives should be byte-identical
sha1=$(sha256sum out1.ociarchive | cut -d' ' -f1)
sha2=$(sha256sum out2.ociarchive | cut -d' ' -f1)
if [[ "${sha1}" != "${sha2}" ]]; then
    echo "ERROR: OCI archives differ between builds"
    echo "Build 1: ${sha1}"
    echo "Build 2: ${sha2}"
    if command -v diffoscope &>/dev/null; then
        diffoscope out1.ociarchive out2.ociarchive || true
    fi
    exit 1
fi
