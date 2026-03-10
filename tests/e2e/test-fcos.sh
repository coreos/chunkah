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

output_dir="${OUTPUT_DIR:?}"
cleanup() {
    cleanup_images "${CHUNKED_IMAGE}"
}
trap cleanup EXIT

# build split image using Containerfile.splitter API
# notice here we --prune /sysroot/
podman pull "${TARGET_IMAGE}"
set +x
config_str=$(podman inspect "${TARGET_IMAGE}")
set -x
buildah_build \
    --from "${TARGET_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg "CHUNKAH_ARGS=-v --prune /sysroot/ --max-layers 96 --write-peak-mem-to /run/output/peak-mem --write-manifest-to /run/output/manifest.json --trace-logfile /run/output/trace.log" \
    -v "${output_dir}:/run/output" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check it
podman run --rm "${CHUNKED_IMAGE}" cat /etc/os-release | grep CoreOS

# check for expected FCOS components
assert_has_components "${CHUNKED_IMAGE}" "rpm/kernel" "rpm/systemd" "rpm/ignition" "rpm/podman"

# verify we got exactly 96 layers
assert_layer_count "${CHUNKED_IMAGE}" 96

# verify unclaimed component is under 5MB (5242880 bytes)
unclaimed_size=$(jq '.components["chunkah/unclaimed"].size' "${output_dir}/manifest.json")
[[ -n "${unclaimed_size}" ]]
if [[ ${unclaimed_size} -gt 5242880 ]]; then
    echo "ERROR: unclaimed size ${unclaimed_size} exceeds 5MB"
    jq '.components["chunkah/unclaimed"]' "${output_dir}/manifest.json"
    exit 1
fi

# verify selinux policy files were reclaimed into the selinux-policy
# component (these files are moved by compose tooling and reclaimed via digest
# matching today but ideally soon just by path canonicalization through the
# /var/lib/selinux symlink)
jq < "${output_dir}/manifest.json" -e \
    '.components["rpm/selinux-policy"].files[]
     | select(test("/etc/selinux/targeted/active/modules/100/systemd/cil"))' > /dev/null
# check the bootupd move to /usr/lib/efi is detected
shim_dir=$(podman run --rm "${CHUNKED_IMAGE}" ls /usr/lib/efi/shim)
jq < "${output_dir}/manifest.json" --arg subdir "${shim_dir}" -e \
    '.components["rpm/shim"].files[]
     | select(test("/usr/lib/efi/shim/" + $subdir + "/EFI/fedora/shim.efi"))' > /dev/null

# verify chunked image is not larger than original + 1%
# (catches possible e.g. bad hardlink handling)
size_original=$(podman image inspect "${TARGET_IMAGE}" | jq '.[0].Size')
size_chunked=$(podman image inspect "${CHUNKED_IMAGE}" | jq '.[0].Size')
max_size=$((size_original * 101 / 100))
[[ ${size_chunked} -le ${max_size} ]]

# verify the chunked image is equivalent to the source (excluding pruned /sysroot/)
assert_no_diff "${TARGET_IMAGE}" "${CHUNKED_IMAGE}" --skip /sysroot/

# verify peak memory is under 200 MiB (209715200 bytes)
peak_mem_bytes=$(cat "${output_dir}/peak-mem")
[[ ${peak_mem_bytes} -le 209715200 ]]

# run bootc lint
podman run --rm "${CHUNKED_IMAGE}" bootc container lint
