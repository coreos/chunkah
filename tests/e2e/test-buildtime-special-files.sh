#!/bin/bash
# Test that --skip-special-files skips FIFO files during image splitting.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

BASE_IMAGE="quay.io/fedora/fedora-minimal:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked-fifo:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

podman pull "${BASE_IMAGE}"

cat > Containerfile.fifo <<EOF
FROM ${BASE_IMAGE} AS builder
RUN mkfifo /tmp/test.fifo

FROM ${CHUNKAH_IMG:?} AS chunkah
RUN --mount=from=builder,src=/,target=/chunkah,ro \\
    --mount=type=bind,target=/run/src,rw \\
        chunkah build --skip-special-files > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
EOF

buildah_build -t "${CHUNKED_IMAGE}" -f Containerfile.fifo .

# verify the FIFO file is NOT present in the final image
if podman run --rm "${CHUNKED_IMAGE}" test -e /tmp/test.fifo; then
    echo "ERROR: FIFO file should not be present in the final image"
    exit 1
fi
