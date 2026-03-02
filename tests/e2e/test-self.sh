#!/bin/bash
# Test the chunkah image itself for proper chunking, catching pathological
# cases like unclaimed files and misattributed bigfiles.
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

CHUNKED_IMAGE="localhost/chunkah-chunked:test"

cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# Check if the image is already chunked by looking for chunkah authorship in the
# image history entries.
is_chunked=$(skopeo inspect --config "containers-storage:${CHUNKAH_IMG:?}" | \
    jq '[.history[]? | select(.author == "chunkah")] | length > 0')

if [[ "${is_chunked}" == "true" ]]; then
    podman tag "${CHUNKAH_IMG}" "${CHUNKED_IMAGE}"
else
    # chunk it using Containerfile.splitter
    config_str=$(podman inspect "${CHUNKAH_IMG}")
    buildah_build \
        --from "${CHUNKAH_IMG}" --build-arg CHUNKAH="${CHUNKAH_IMG}" \
        --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
        --build-arg CHUNKAH_ARGS="-v" \
        -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"
fi

# verify minimum layer count
layer_count=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | jq '.LayersData | length')
if [[ ${layer_count} -lt 32 ]]; then
    echo "ERROR: Expected at least 32 layers, got ${layer_count} in ${CHUNKED_IMAGE}"
    exit 1
fi

# check for expected RPM components
assert_has_components "${CHUNKED_IMAGE}" "rpm/glibc" "rpm/openssl"

# Verify no unexpected bigfiles; any not in the allowlist (e.g. libc.so.6,
# libcrypto.so) would indicate RPM database read failures; this was a bug early
# on when testing against UBI10 due to PQC RPM signatures.
annotations=$(get_layer_annotations "${CHUNKED_IMAGE}")
bigfiles=$(grep '^bigfiles/' <<< "${annotations}")
while IFS= read -r component; do
    case "${component}" in
        ""|bigfiles/chunkah|bigfiles/rpmdb.sqlite) continue;;
        *) echo "ERROR: Unexpected bigfile '${component}' in ${CHUNKED_IMAGE}"; exit 1;;
    esac
done <<< "${bigfiles}"

# verify the unclaimed layer is small (< 1 MiB)
unclaimed_size=$(skopeo inspect "containers-storage:${CHUNKED_IMAGE}" | \
    jq '.LayersData[] | select(.Annotations["org.chunkah.component"] | contains("chunkah/unclaimed")) | .Size')
[[ -n "${unclaimed_size}" ]]
[[ ${unclaimed_size} -le 1048576 ]]
