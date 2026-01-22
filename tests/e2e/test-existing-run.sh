#!/bin/bash
# Test splitting an existing image using podman run with image mounts.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

SOURCE_IMAGE="localhost/fedora:test"
CHUNKED_IMAGE="localhost/fedora-chunked:test"

cleanup() {
    cleanup_images "${SOURCE_IMAGE}" "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# build a derived image so we can test file cap handling
podman build -t "${SOURCE_IMAGE}" -f - <<'EOF'
FROM quay.io/fedora/fedora:latest
# create a test binary and set a capability on it
RUN cp /usr/bin/true /usr/bin/test-caps && setcap cap_net_raw+ep /usr/bin/test-caps
EOF
CHUNKAH_CONFIG_STR=$(podman inspect "${SOURCE_IMAGE}")

# run chunkah!
podman run --rm --mount=type=image,src="${SOURCE_IMAGE}",target=/chunkah \
  -e CHUNKAH_CONFIG_STR="${CHUNKAH_CONFIG_STR}" \
      "${CHUNKAH_IMG:?}" build > out.ociarchive

# XXX: need to fix 'podman load' to only print image ID on its stdout, like 'podman pull'
iid=$(podman load -i out.ociarchive)
iid=${iid#*sha256:}
podman tag "${iid}" "${CHUNKED_IMAGE}"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep Fedora

# check for expected components
assert_has_components "${CHUNKED_IMAGE}" "rpm/filesystem" "rpm/setup" "rpm/glibc"

# sanity-check we got at least 16 layers
assert_min_layers "${CHUNKED_IMAGE}" 16

# verify that security.capability xattrs are preserved
caps=$(podman run --rm "${CHUNKED_IMAGE}" getcap /usr/bin/test-caps)
[[ "${caps}" == *"cap_net_raw=ep"* ]]
