#!/bin/bash
# Test splitting a Fedora CoreOS image.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

TARGET_IMAGE="quay.io/fedora/fedora-coreos:stable"
CHUNKED_IMAGE="localhost/fcos-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# build split image using Containerfile.splitter API
# notice here we --prune /sysroot/
podman pull "${TARGET_IMAGE}"
config_str=$(podman inspect "${TARGET_IMAGE}")
buildah_build \
    --from "${TARGET_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg "CHUNKAH_ARGS=-v --prune /sysroot/ --max-layers 96" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep CoreOS

# check for expected FCOS components
assert_has_components "${CHUNKED_IMAGE}" "rpm/kernel" "rpm/systemd" "rpm/ignition" "rpm/podman"

# verify we got exactly 96 layers
assert_layer_count "${CHUNKED_IMAGE}" 96

# verify layer containing unclaimed is under 100MB (104857600 bytes)
unclaimed_size=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | \
    jq '.LayersData[] | select(.Annotations["org.chunkah.component"] | contains("chunkah/unclaimed")) | .Size')
[[ -n "${unclaimed_size}" ]]
[[ ${unclaimed_size} -le 104857600 ]]

# verify chunked image is not larger than original + 1%
# (catches possible e.g. bad hardlink handling)
size_original=$(podman image inspect "${TARGET_IMAGE}" | jq '.[0].Size')
size_chunked=$(podman image inspect "${CHUNKED_IMAGE}" | jq '.[0].Size')
max_size=$((size_original * 101 / 100))
[[ ${size_chunked} -le ${max_size} ]]

# verify the chunked image is equivalent to the source (excluding pruned /sysroot/)
assert_no_diff "${TARGET_IMAGE}" "${CHUNKED_IMAGE}" --skip /sysroot/

# run bootc lint
podman run --rm "${CHUNKED_IMAGE}" bootc container lint
