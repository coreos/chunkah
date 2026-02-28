#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit

cd "$(dirname "$0")"

echo ">>> REGENERATING: fedora.qf" >&2

# Packages to cherry-pick from the full rpm -qa output.
# Some of these are not in fedora-minimal and need to be installed first.
PACKAGES=(
    bash
    coreutils
    fedora-release-common
    glibc
    langpacks-core-en
    perl-POSIX
    rpm
    setup
    shadow-utils
    util-linux-core
)

# The queryformat string must match QUERYFORMAT in rpm-qa-rs src/parse.rs.
queryformat='@@PKG@@\t%{NAME}\t%{VERSION}\t%{RELEASE}\t%{EPOCH}\t%{ARCH}\t%{LICENSE}\t%{SIZE}\t%{BUILDTIME}\t%{INSTALLTIME}\t%{SOURCERPM}\t%{FILEDIGESTALGO}\n[@@FILE@@\t%{FILENAMES}\t%{FILESIZES}\t%{FILEMODES}\t%{FILEMTIMES}\t%{FILEDIGESTS}\t%{FILEFLAGS}\t%{FILEUSERNAME}\t%{FILEGROUPNAME}\t%{FILELINKTOS}\n][@@CL@@\t%{CHANGELOGTIME}\n]'

# Sort package names for deterministic output
sorted_output=$(printf '%s\n' "${PACKAGES[@]}" | sort)
mapfile -t sorted <<< "${sorted_output}"

podman run --rm quay.io/fedora/fedora-minimal:latest \
    bash -c '
        set -euo pipefail
        dnf install -y --setopt=install_weak_deps=False "$@" >/dev/null
        for pkg in "$@"; do rpm -q --queryformat "'"${queryformat}"'" "$pkg"; done
    ' -- "${sorted[@]}" > fedora.qf

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
