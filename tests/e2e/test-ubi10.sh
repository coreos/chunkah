#!/bin/bash
# Test splitting a UBI10 image. This exercises the PQC-enabled rpm-sequoia
# installed from CentOS Stream 10 to verify we can read the rpmdb of EL10
# images with post-quantum signatures.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

SOURCE_IMAGE="registry.access.redhat.com/ubi10/ubi:latest"
CHUNKED_IMAGE="localhost/ubi10-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

podman pull "${SOURCE_IMAGE}"
set +x
config_str=$(podman inspect "${SOURCE_IMAGE}")
set -x
buildah_build \
    --from "${SOURCE_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg "CHUNKAH_ARGS=-v" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep 'Red Hat Enterprise Linux'

# check for expected RPM components signed by PQC
assert_has_components "${CHUNKED_IMAGE}" "rpm/glibc" "rpm/openssl"

# verify we got exactly 64 layers (the default)
assert_layer_count "${CHUNKED_IMAGE}" 64

# verify the chunked image is equivalent to the source
assert_no_diff "${SOURCE_IMAGE}" "${CHUNKED_IMAGE}"
