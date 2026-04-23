#!/bin/bash
# Creates source and vendor tarballs for chunkah.
# Used by both release.py and Packit's create-archive action.
# Usage: create-archives.sh <source-tarball> <vendor-tarball> [commit]
#        create-archives.sh --outdir <outdir>
set -euo pipefail
shopt -s inherit_errexit

version=$(cargo metadata --no-deps --format-version=1 \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

commit="HEAD"
if [[ "${1:-}" == "--outdir" ]]; then
    outdir="${2:-.}"
    source_tarball="${outdir}/chunkah-${version}.tar.gz"
    vendor_tarball="${outdir}/chunkah-${version}-vendor.tar.gz"
elif [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "Usage: create-archives.sh <source-tarball> <vendor-tarball> [commit]" >&2
    echo "       create-archives.sh --outdir <outdir>" >&2
    exit 1
else
    source_tarball="${1}"
    vendor_tarball="${2}"
    commit="${3:-${commit}}"
fi

git archive --format=tar.gz "--prefix=chunkah-${version}/" \
    -o "${source_tarball}" "${commit}"

cargo vendor-filterer --platform '*-unknown-linux-gnu' --tier 2 \
    --format=tar.gz --prefix=vendor "${vendor_tarball}"

echo "${source_tarball}"
