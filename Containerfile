# Use `just buildimg` to build this, or:
#
#   buildah build --skip-unused-stages=false -t chunkah .

ARG BASE=quay.io/fedora/fedora-minimal:43
ARG DNF_FLAGS="-y --setopt=install_weak_deps=False"
ARG CACHE_ID=chunkah-target

FROM ${BASE} AS builder
ARG DNF_FLAGS
ARG CACHE_ID
RUN --mount=type=cache,rw,id=dnf,target=/var/cache/libdnf5 \
    dnf install ${DNF_FLAGS} cargo rust pkg-config openssl-devel zlib-devel
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,rw,id=cargo,target=/root/.cargo \
    --mount=type=cache,rw,id=${CACHE_ID},target=/build/target \
    cargo build --release && cp /build/target/release/chunkah /usr/bin

# XXX: Temporary hack until Fedora learns to read PQC sigs from el10 images.
FROM quay.io/centos/centos:stream10 AS c10s
RUN dnf download -y --repo baseos rpm-sequoia --destdir=/rpms

FROM ${BASE} AS rootfs
ARG DNF_FLAGS
RUN --mount=type=cache,id=dnf,target=/mnt \
    cp -a /mnt /var/cache/libdnf5 && \
    dnf install ${DNF_FLAGS} openssl zlib && rm -rf /var/cache/*
COPY --from=c10s /rpms/ /tmp/rpms/
RUN rpm -Uvh --oldpackage /tmp/rpms/rpm-sequoia-*.rpm && rm -rf /tmp/rpms
COPY --from=builder /usr/bin/chunkah /usr/bin/chunkah
# Repeat inline config below for the `--no-chunk` flow. See related XXX below.
ENTRYPOINT ["/usr/bin/chunkah"]
ENV CHUNKAH_ROOTFS=/chunkah
WORKDIR /srv

FROM rootfs AS rechunk
ARG DNF_FLAGS
RUN --mount=type=cache,rw,id=dnf,target=/var/cache/libdnf5 \
    dnf install ${DNF_FLAGS} sqlite
COPY --from=rootfs / /rootfs
RUN for db in /rootfs/var/lib/rpm/rpmdb.sqlite \
              /rootfs/usr/lib/sysimage/libdnf5/transaction_history.sqlite \
              /rootfs/var/lib/dnf/history.sqlite; do \
        if [ -f "${db}" ]; then sqlite3 "${db}" "PRAGMA journal_mode = DELETE;"; fi; \
    done
## XXX: Work around https://github.com/containers/buildah/issues/6652 for
## our own image for now by just passing a config manually rather than using
## Containerfile directives in the final stage.
RUN --mount=type=bind,target=/run/src,rw \
    chunkah build --rootfs /rootfs \
        --config-str '{"Config": {"Entrypoint": ["/usr/bin/chunkah"], "Env": ["CHUNKAH_ROOTFS=/chunkah"], "WorkingDir": "/srv"}}' \
        > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
