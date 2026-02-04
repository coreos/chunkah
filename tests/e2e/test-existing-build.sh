#!/bin/bash
# Test splitting an existing image using Containerfile.splitter.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

TARGET_IMAGE="quay.io/fedora/fedora-minimal:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# build split image using Containerfile.splitter API
podman pull "${TARGET_IMAGE}"
config_str=$(podman inspect "${TARGET_IMAGE}")
buildah_build \
    --from "${TARGET_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg CHUNKAH_ARGS="-v" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep Fedora

# check for expected components
assert_has_components "${CHUNKED_IMAGE}" "rpm/filesystem" "rpm/setup" "rpm/glibc"

# verify we got exactly 64 layers (the default)
assert_layer_count "${CHUNKED_IMAGE}" 64

# verify the chunked image is equivalent to the source
assert_no_diff "${TARGET_IMAGE}" "${CHUNKED_IMAGE}"
