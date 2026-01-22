#!/bin/bash
# Test image splitting at build time using the FROM oci-archive: trick.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

BASE_IMAGE="quay.io/fedora/fedora-minimal:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

podman pull "${BASE_IMAGE}"

cat > Containerfile <<EOF
FROM ${BASE_IMAGE} AS builder
RUN microdnf install -y jq && microdnf clean all
# create a test binary and set a capability on it
RUN cp /usr/bin/true /usr/bin/test-caps && setcap cap_net_raw+ep /usr/bin/test-caps

FROM ${CHUNKAH_IMG:?} AS chunkah
RUN --mount=from=builder,src=/,target=/chunkah,ro \\
    --mount=type=bind,target=/run/src,rw \\
        chunkah build > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
EOF

buildah_build -t "${CHUNKED_IMAGE}" -f Containerfile .

# verify jq works in the resulting image
podman run --rm "${CHUNKED_IMAGE}" jq --version

# check for expected components
assert_has_components "${CHUNKED_IMAGE}" "rpm/filesystem" "rpm/setup" "rpm/glibc" "rpm/jq"

# sanity-check we got at least 16 layers
assert_min_layers "${CHUNKED_IMAGE}" 16

# verify that security.capability xattrs are preserved
caps=$(podman run --rm "${CHUNKED_IMAGE}" getcap /usr/bin/test-caps)
[[ "${caps}" == *"cap_net_raw=ep"* ]]
