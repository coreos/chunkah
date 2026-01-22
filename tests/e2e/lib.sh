#!/bin/bash
# Shared helper functions for e2e tests.

# shellcheck disable=SC2312

# Run buildah build with --skip-unused-stages=false and workarounds for older buildah versions.
buildah_build() {
    local tmp_args=()
    local version min_version
    version=$(${BUILDAH:-buildah} version --json | jq -r '.version')
    min_version=$(echo -e "${version}\n1.43" | sort -V | head -n1)
    if [[ "${min_version}" != "1.43" ]]; then
        tmp_args+=(-v "${PWD}:/run/src" --security-opt=label=disable)
    fi
    ${BUILDAH:-buildah} build --skip-unused-stages=false "${tmp_args[@]}" "$@"
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
            exit 1
        fi
    done
}

# Assert that an image has at least the expected number of layers.
assert_min_layers() {
    local image="${1}"; shift
    local min_count="${1}"; shift
    local count
    count=$(skopeo inspect "containers-storage:${image}" | jq '.LayersData | length')
    if [[ ${count} -lt ${min_count} ]]; then
        echo "ERROR: Expected at least ${min_count} layers, got ${count} in ${image}"
        exit 1
    fi
}

# Remove images, ignoring errors.
cleanup_images() {
    for image in "$@"; do
        podman rmi -f "${image}" 2>/dev/null || true
    done
}
