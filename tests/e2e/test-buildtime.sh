#!/bin/bash
# Test image splitting at build time using the FROM oci-archive: trick.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

BASE_IMAGE="quay.io/fedora/fedora-minimal:latest"
ROOTFS_IMAGE="localhost/test-buildtime-rootfs:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked:test"
CHUNKED_IMAGE2="localhost/fedora-minimal-chunked:test2"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}" "${CHUNKED_IMAGE2}" "${ROOTFS_IMAGE}"
}
trap cleanup EXIT

podman pull "${BASE_IMAGE}"

# Build rootfs as a separate image so that the reproducibility check below
# operates on the same rootfs both times. Without this, each buildah_build
# would re-run dnf install, producing different file timestamps.
cat > Containerfile.rootfs <<EOF
FROM ${BASE_IMAGE}
RUN dnf install -y jq acl attr && dnf clean all
EOF
buildah build -t "${ROOTFS_IMAGE}" -f Containerfile.rootfs .

# build chunked image using FROM oci-archive: trick
cat > Containerfile <<EOF
FROM ${ROOTFS_IMAGE} AS builder
# create a test binary with various xattr types
RUN cp /usr/bin/true /usr/bin/test-xattrs && \
    setcap cap_net_raw+ep /usr/bin/test-xattrs && \
    setfacl -m u:nobody:r /usr/bin/test-xattrs && \
    setfattr -n user.testkey1 -v testvalue1 /usr/bin/test-xattrs && \
    setfattr -n user.testkey2 -v testvalue2 /usr/bin/test-xattrs

FROM ${CHUNKAH_IMG:?} AS chunkah
RUN --mount=from=builder,src=/,target=/chunkah,ro \\
    --mount=type=bind,target=/run/src,rw \\
    SOURCE_DATE_EPOCH=1700000000 \\
        chunkah build -v > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
EOF

buildah_build -t "${CHUNKED_IMAGE}" -f Containerfile .

# verify jq works in the resulting image
podman run --rm "${CHUNKED_IMAGE}" jq --version

# check for expected components
assert_has_components "${CHUNKED_IMAGE}" "rpm/filesystem" "rpm/setup" "rpm/glibc" "rpm/jq"

# verify we got exactly 64 layers (the default)
assert_layer_count "${CHUNKED_IMAGE}" 64

# verify that security.capability xattrs are preserved
caps=$(podman run --rm "${CHUNKED_IMAGE}" getcap /usr/bin/test-xattrs)
[[ "${caps}" == *"cap_net_raw=ep"* ]]

# verify that POSIX ACLs (system.posix_acl_access) are preserved
acl=$(podman run --rm "${CHUNKED_IMAGE}" getfacl -c /usr/bin/test-xattrs)
[[ "${acl}" == *"user:nobody:r--"* ]]

# verify that user.* xattrs are preserved
xattr1=$(podman run --rm "${CHUNKED_IMAGE}" getfattr -n user.testkey1 --only-values /usr/bin/test-xattrs)
[[ "${xattr1}" == "testvalue1" ]]
xattr2=$(podman run --rm "${CHUNKED_IMAGE}" getfattr -n user.testkey2 --only-values /usr/bin/test-xattrs)
[[ "${xattr2}" == "testvalue2" ]]

# verify reproducibility: build again with --no-cache to force chunkah to
# re-run on the same rootfs and compare image IDs; this validates that xattr
# sorting and trusted.* filtering produce deterministic output
buildah_build --no-cache -t "${CHUNKED_IMAGE2}" -f Containerfile .

id1=$(podman inspect --format '{{.Id}}' "${CHUNKED_IMAGE}")
id2=$(podman inspect --format '{{.Id}}' "${CHUNKED_IMAGE2}")
if [[ "${id1}" != "${id2}" ]]; then
    echo "ERROR: image IDs differ between builds"
    echo "Build 1: ${id1}"
    echo "Build 2: ${id2}"
    # this will fail; it's just a way to call into `just diff` to print the diff
    assert_no_diff "${CHUNKED_IMAGE}" "${CHUNKED_IMAGE2}"
    exit 1 # but just in case
fi
