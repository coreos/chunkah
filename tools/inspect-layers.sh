#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit

# Inspect layers of a chunkah-split image, showing size, stability, and components.

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS] <image>

Inspect layers of a chunkah-split image, showing size, stability, and components.

Options:
    -n, --top N            Show only the top N layers (default: all)
    -s, --sort-by FIELD    Sort by field: size (default) or stability
    -r, --reverse          Reverse the sort order
    -h, --help             Show this help message

Arguments:
    image                  Image reference (e.g. containers-storage:localhost/myimage:latest,
                           oci-archive:/path/to/image.ociarchive)

Examples:
    $(basename "$0") containers-storage:localhost/fcos-chunked:latest
    $(basename "$0") oci-archive:/tmp/image.ociarchive
    $(basename "$0") --top 20 containers-storage:localhost/fcos-chunked:latest
    $(basename "$0") --sort-by stability oci-archive:/tmp/image.ociarchive
EOF
}

# Parse options
top_n=""
sort_by="size"
reverse=false
while [[ $# -gt 0 ]]; do
    case "${1}" in
        -n|--top)
            top_n="${2}"
            shift 2
            ;;
        -s|--sort-by)
            sort_by="${2}"
            if [[ "${sort_by}" != "size" && "${sort_by}" != "stability" ]]; then
                echo "Error: --sort-by must be 'size' or 'stability'" >&2
                exit 1
            fi
            shift 2
            ;;
        -r|--reverse)
            reverse=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        -*)
            echo "Unknown option: ${1}" >&2
            usage >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

if [[ $# -ne 1 ]]; then
    usage >&2
    exit 1
fi

image="${1}"

# Get layer data from skopeo
layer_data=$(skopeo inspect "${image}" | jq -r '
    .LayersData[] |
    {
        size: .Size,
        component: (.Annotations["org.chunkah.component"] // "unknown"),
        stability: (.Annotations["org.chunkah.stability"] // "unknown")
    }
')

if [[ -z "${layer_data}" ]]; then
    echo "Error: Could not inspect image or no layers found" >&2
    exit 1
fi

# Count layers
layer_count=$(echo "${layer_data}" | jq -s 'length')

echo "=== Chunkah Layer Breakdown ==="
echo ""
echo "Total layers: ${layer_count}"
echo ""
printf "%-12s %-10s %-60s\n" "Size (MB)" "Stability" "Components"
printf "%-12s %-10s %-60s\n" "----------" "---------" "------------------------------------------------------------"

# Sort and optionally limit layers
sort_and_limit() {
    local sort_flags="-n"
    if [[ "${sort_by}" == "stability" ]]; then
        # Sort by stability (column 2), descending by default (most stable first)
        sort_flags="-k2 -n"
        if [[ "${reverse}" == "false" ]]; then
            sort_flags="${sort_flags}r"
        fi
    else
        # Sort by size (column 1), descending by default (largest first)
        sort_flags="-k1 -n"
        if [[ "${reverse}" == "false" ]]; then
            sort_flags="${sort_flags}r"
        fi
    fi
    # shellcheck disable=SC2086
    sort -t'	' ${sort_flags} | if [[ -n "${top_n}" ]]; then head -n "${top_n}"; else cat; fi
}

echo "${layer_data}" | jq -r '[.size, .stability, .component] | @tsv' | \
    sort_and_limit | \
    while IFS=$'\t' read -r size stability comps; do
        size_mb=$(printf "%.2f" "$(echo "${size} / 1024 / 1024" | bc -l)" || true)

        # Truncate long component lists
        if [[ ${#comps} -gt 58 ]]; then
            comp_count=$(echo "${comps}" | wc -w)
            first_comps=$(echo "${comps}" | cut -d' ' -f1-3)
            comps="${first_comps} ... (+$(( comp_count - 3 )) more)"
        fi

        # Format stability (truncate to 3 decimal places if numeric)
        if [[ "${stability}" =~ ^[0-9.]+$ ]]; then
            stability=$(printf "%.3f" "${stability}")
        fi

        printf "%10s   %-10s %s\n" "${size_mb}" "${stability}" "${comps}"
    done
