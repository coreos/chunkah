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

output_dir="${OUTPUT_DIR:?}"
cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# chunk it using Containerfile.splitter
config_str=$(podman inspect "${CHUNKAH_IMG:?}")
buildah_build \
    --from "${CHUNKAH_IMG}" --build-arg CHUNKAH="${CHUNKAH_IMG}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg "CHUNKAH_ARGS=-v --write-manifest-to /run/output/manifest.json" \
    -v "${output_dir}:/run/output" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

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

# verify unclaimed component is under 1 MiB (1048576 bytes)
unclaimed_size=$(jq '.components["chunkah/unclaimed"].size' "${output_dir}/manifest.json")
[[ -n "${unclaimed_size}" ]]
[[ ${unclaimed_size} -le 1048576 ]]
