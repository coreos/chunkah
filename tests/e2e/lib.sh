#!/bin/bash
# Shared helper functions for e2e tests.

# shellcheck disable=SC2312

# Run buildah build with --skip-unused-stages=false and workarounds for older buildah versions.
buildah_build() {
    local tmp_args=()
    local version min_version
    version=$(${BUILDAH:-buildah} version --json | jq -r '.version')
    min_version=$(echo -e "${version}\n1.44" | sort -V | head -n1)
    if [[ "${min_version}" != "1.44" ]]; then
        tmp_args+=(-v "${PWD}:/run/src" --security-opt=label=disable)
    fi
    ${BUILDAH:-buildah} build --skip-unused-stages=false "${tmp_args[@]}" "$@"
    rm -f "${PWD}/out.ociarchive"
}

# Get layer annotations for an image.
get_layer_annotations() {
    local image="${1}"
    skopeo inspect "containers-storage:${image}" | \
        jq -r '.LayersData[].Annotations["org.chunkah.component"] // empty'
}

# Assert that an image has the expected components in its layer annotations.
assert_has_components() {
    local image="${1}"; shift
    local annotations
    annotations=$(get_layer_annotations "${image}")
    for component in "$@"; do
        if ! grep -q "${component}" <<< "${annotations}"; then
            echo "ERROR: Expected component '${component}' not found in ${image}"
            return 1
        fi
    done
}

# Assert that an image has exactly the expected number of layers.
assert_layer_count() {
    local image="${1}"; shift
    local expected="${1}"; shift
    local count
    count=$(skopeo inspect "containers-storage:${image}" | jq '.LayersData | length')
    if [[ ${count} -ne ${expected} ]]; then
        echo "ERROR: Expected ${expected} layers, got ${count} in ${image}"
        return 1
    fi
}

# Assert that a path exists in an image.
assert_path_exists() {
    local image="${1}"; shift
    local path="${1}"; shift
    if ! podman run --rm "${image}" test -e "${path}"; then
        echo "ERROR: ${path} should exist in ${image}"
        return 1
    fi
}

# Assert that a path does not exist in an image.
assert_path_not_exists() {
    local image="${1}"; shift
    local path="${1}"; shift
    if podman run --rm "${image}" test -e "${path}"; then
        echo "ERROR: ${path} should not exist in ${image}"
        return 1
    fi
}

# Assert that two images have no filesystem differences (ignoring timestamps).
# Additional arguments are passed through to chunkah-differ (e.g. --skip /sysroot/).
assert_no_diff() {
    local repo_root
    repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    just -f "${repo_root}/Justfile" diff "$@"
}

# Remove images, ignoring errors.
cleanup_images() {
    for image in "$@"; do
        podman rmi -f "${image}" 2>/dev/null || true
    done
}
