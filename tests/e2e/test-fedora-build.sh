#!/bin/bash
# Test splitting an existing image using Containerfile.splitter.
set -xeuo pipefail
shopt -s inherit_errexit

TARGET_IMAGE="quay.io/fedora/fedora-minimal:latest"
CHUNKED_IMAGE="localhost/fedora-minimal-chunked:test"

# Find repo root from script location
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

cleanup() {
    podman rmi -f "${CHUNKED_IMAGE}" 2>/dev/null || true
}
trap cleanup EXIT

tmp_args=()
version=$(${BUILDAH:-buildah} version --json | jq -r '.version')
min_version=$(echo -e "${version}\n1.43" | sort -V | head -n1)
if [[ "${min_version}" != "1.43" ]]; then
    tmp_args+=(-v "${PWD}:/run/src" --security-opt=label=disable)
fi

# build split image using Containerfile.splitter API
podman pull "${TARGET_IMAGE}"
config_str=$(podman inspect "${TARGET_IMAGE}")
${BUILDAH:-buildah} build --skip-unused-stages=false \
    --from "${TARGET_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    -t "${CHUNKED_IMAGE}" "${tmp_args[@]}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep Fedora

# use skopeo to inspect the ociarchive and check for layer annotations
layer_annotations=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | \
    jq -r '.LayersData[].Annotations["org.chunkah.component"] // empty')

# check for some expected components
grep -q "rpm/filesystem" <<< "${layer_annotations}"
grep -q "rpm/setup" <<< "${layer_annotations}"
grep -q "rpm/glibc" <<< "${layer_annotations}"

# sanity-check we got at least 16 components
n=$(wc -l <<< "${layer_annotations}")
[[ ${n} -gt 16 ]]
