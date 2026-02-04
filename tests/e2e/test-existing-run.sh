#!/bin/bash
# Test splitting an existing image using podman run with image mounts.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

SOURCE_IMAGE="quay.io/fedora/fedora-minimal:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

podman pull "${SOURCE_IMAGE}"
CHUNKAH_CONFIG_STR=$(podman inspect "${SOURCE_IMAGE}")

# run chunkah!
podman run --rm --mount=type=image,src="${SOURCE_IMAGE}",target=/chunkah \
  -e CHUNKAH_CONFIG_STR="${CHUNKAH_CONFIG_STR}" \
      "${CHUNKAH_IMG:?}" build -v > out.ociarchive

# XXX: need to fix 'podman load' to only print image ID on its stdout, like 'podman pull'
iid=$(podman load -i out.ociarchive)
iid=${iid#*sha256:}
podman tag "${iid}" "${CHUNKED_IMAGE}"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep Fedora

# check for expected components
assert_has_components "${CHUNKED_IMAGE}" "rpm/filesystem" "rpm/setup" "rpm/glibc"

# verify we got exactly 64 layers (the default)
assert_layer_count "${CHUNKED_IMAGE}" 64

# verify the chunked image is equivalent to the source
assert_no_diff "${SOURCE_IMAGE}" "${CHUNKED_IMAGE}"
