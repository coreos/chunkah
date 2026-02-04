#!/bin/bash
# Test various build options during image splitting.
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
RUN mkdir -p /prune-me/nested && echo "should be gone" > /prune-me/nested/file.txt
RUN mkdir -p /prune-children/nested && echo "should be gone" > /prune-children/nested/file.txt

FROM ${CHUNKAH_IMG:?} AS chunkah
RUN --mount=from=builder,src=/,target=/chunkah,ro \\
    --mount=type=bind,target=/run/src,rw \\
        chunkah build -v \\
            --skip-special-files \\
            --prune /prune-me \\
            --prune /prune-children/ \\
                > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
EOF

buildah_build -t "${CHUNKED_IMAGE}" -f Containerfile.fifo .

# verify the FIFO file is NOT present in the final image
assert_path_not_exists "${CHUNKED_IMAGE}" /tmp/test.fifo

# verify --prune /prune-me removed the directory entirely
assert_path_not_exists "${CHUNKED_IMAGE}" /prune-me

# verify --prune /prune-children/ kept the directory but removed its contents
assert_path_exists "${CHUNKED_IMAGE}" /prune-children
assert_path_not_exists "${CHUNKED_IMAGE}" /prune-children/nested

# verify we got exactly 64 layers (the default)
assert_layer_count "${CHUNKED_IMAGE}" 64
