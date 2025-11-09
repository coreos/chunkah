#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit

cd "$(dirname "$0")"

echo ">>> REGENERATING: fedora.json" >&2

# Packages to cherry-pick from the full rpm -qa output
PACKAGES=(
    bash
    glibc
    coreutils
    rpm
    shadow-utils
    util-linux-core
    setup
    perl-POSIX
)

# Build jq filter to select packages by name
filter=$(printf '"%s",' "${PACKAGES[@]}")
filter="[${filter%,}]"

podman run --rm quay.io/fedora/fedora-minimal:latest rpm -qa --json | \
    jq -s --argjson names "${filter}" '
        [.[] | select(.Name as $n | $names | index($n))] | sort_by(.Name) | .[]
    ' > fedora.json

echo ">>> REGENERATING: empty.image-config.json" >&2
buildah build --omit-history -f empty.Containerfile -t chunkah-empty
podman inspect chunkah-empty | jq '.[0].Config' > empty.image-config.json
podman rmi chunkah-empty

echo ">>> REGENERATING rpmdb.sqlite" >&2
podman rm -f chunkah-test-fixture-tmp
podman run --name chunkah-test-fixture-tmp --rm quay.io/hummingbird-ci/builder bash -c '
    dnf install --installroot /mnt -y --use-host-config --nodocs --setopt=install_weak_deps=False filesystem setup &>2
    sqlite3 /mnt/usr/lib/sysimage/rpm/rpmdb.sqlite "PRAGMA journal_mode = DELETE;" &>2
    cat /mnt/usr/lib/sysimage/rpm/rpmdb.sqlite
' > rpmdb.sqlite
