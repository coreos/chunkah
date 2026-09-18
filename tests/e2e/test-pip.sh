#!/bin/bash
# Test the pip components repo with packages installed by pip (system
# site-packages) and uv (venv).
set -xeuo pipefail
shopt -s inherit_errexit

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=SCRIPTDIR/lib.sh
. "${SCRIPT_DIR}/lib.sh"

BASE_IMAGE="quay.io/fedora/fedora-minimal:latest"
ROOTFS_IMAGE="localhost/test-pip-rootfs:latest"
CHUNKED_IMAGE="localhost/fedora-pip-chunked:test"

output_dir="${OUTPUT_DIR:?}"
cleanup() {
    cleanup_images "${CHUNKED_IMAGE}" "${ROOTFS_IMAGE}"
}
trap cleanup EXIT

podman pull "${BASE_IMAGE}"

# python3-requests is rpm-installed and must stay with rpm. numpy is
# pip-installed into system site-packages (pip records its bytecode in
# RECORD). A venv gets packages via uv, which compiles bytecode but does
# not record it. setuptools vendors distributions with their own dist-info.
cat > Containerfile.rootfs <<EOF
FROM ${BASE_IMAGE}
RUN dnf install -y python3-pip python3-requests uv && dnf clean all && \
    pip install --no-cache-dir numpy && \
    uv venv /opt/venv && \
    uv pip install --python /opt/venv --no-cache --compile-bytecode markdown-it-py rich setuptools
EOF
buildah build -t "${ROOTFS_IMAGE}" -f Containerfile.rootfs .

config_str=$(podman inspect "${ROOTFS_IMAGE}")
buildah_build \
    --from "${ROOTFS_IMAGE}" --build-arg CHUNKAH="${CHUNKAH_IMG:?}" \
    --build-arg CHUNKAH_CONFIG_STR="${config_str}" \
    --build-arg "CHUNKAH_ARGS=--max-layers 96 --write-manifest-to /run/output/manifest.json" \
    -v "${output_dir}:/run/output" \
    -t "${CHUNKED_IMAGE}" "${REPO_ROOT}/Containerfile.splitter"

# sanity-check the packages work
podman run --rm "${CHUNKED_IMAGE}" python3 -c "import numpy, requests"
podman run --rm "${CHUNKED_IMAGE}" /opt/venv/bin/python -c "import rich, markdown_it"

# pip and uv installed packages become pip components
assert_has_components "${CHUNKED_IMAGE}" "pip/numpy" "pip/rich" "pip/markdown-it-py" "pip/setuptools" "rpm/python-requests"

# rpm-installed python packages stay with rpm
manifest="${output_dir}/manifest.json"
has_requests=$(jq '.components | has("pip/requests")' "${manifest}")
[[ "${has_requests}" == "false" ]]

# numpy files, including its recorded bytecode and console script, are claimed
numpy_files=$(jq -r '.components["pip/numpy"].files[]' "${manifest}")
grep -q '/site-packages/numpy/__init__.py$' <<< "${numpy_files}"
grep -q '/site-packages/numpy/__pycache__/__init__.cpython-[0-9]*.pyc$' <<< "${numpy_files}"
grep -q '^/usr/local/bin/f2py$' <<< "${numpy_files}"

# rich files, including bytecode that is not in RECORD, are claimed
rich_files=$(jq -r '.components["pip/rich"].files[]' "${manifest}")
grep -q '/opt/venv/lib/python3.*/site-packages/rich/__init__.py$' <<< "${rich_files}"
grep -q '/opt/venv/lib/python3.*/site-packages/rich/__pycache__/__init__.cpython-[0-9]*.pyc$' <<< "${rich_files}"

# distributions vendored inside setuptools belong to setuptools
has_packaging=$(jq '.components | has("pip/packaging")' "${manifest}")
[[ "${has_packaging}" == "false" ]]
setuptools_files=$(jq -r '.components["pip/setuptools"].files[]' "${manifest}")
grep -q '/site-packages/setuptools/_vendor/packaging-[0-9.]*.dist-info/RECORD$' <<< "${setuptools_files}"

# verify the chunked image is equivalent to the source
assert_no_diff "${ROOTFS_IMAGE}" "${CHUNKED_IMAGE}"
