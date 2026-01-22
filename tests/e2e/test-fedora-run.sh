#!/bin/bash
# Test splitting an existing image using podman run with image mounts.
set -xeuo pipefail
shopt -s inherit_errexit

SOURCE_IMAGE="localhost/fedora:test"
CHUNKED_IMAGE="localhost/fedora-chunked:test"

cleanup() {
    podman rmi -f "${SOURCE_IMAGE}" 2>/dev/null || true
    podman rmi -f "${CHUNKED_IMAGE}" 2>/dev/null || true
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

# use skopeo to inspect the ociarchive and check for layer annotations
layer_annotations=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | \
    jq -r '.LayersData[].Annotations["org.chunkah.component"] // empty')

# check for some expected components
grep -q "rpm/filesystem" <<< "${layer_annotations}"
grep -q "rpm/setup" <<< "${layer_annotations}"
grep -q "rpm/glibc" <<< "${layer_annotations}"

# sanity-check we got at least 16 layers
n=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | jq '.LayersData | length')
[[ ${n} -ge 16 ]]

# verify that security.capability xattrs are preserved
caps=$(podman run --rm "${CHUNKED_IMAGE}" getcap /usr/bin/test-caps)
[[ "${caps}" == *"cap_net_raw=ep"* ]]
