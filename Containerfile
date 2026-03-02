# Use `just buildimg` to build this, or:
#
#   buildah build --skip-unused-stages=false -t chunkah .

ARG BASE=quay.io/fedora/fedora-minimal:43
ARG FINAL_FROM=oci-archive:out.ociarchive
ARG DNF_FLAGS="-y --setopt=install_weak_deps=False"

FROM ${BASE} AS builder
ARG DNF_FLAGS
RUN --mount=type=cache,rw,id=dnf,target=/var/cache/libdnf5 \
    dnf install ${DNF_FLAGS} cargo rust pkg-config openssl-devel zlib-devel
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,rw,id=cargo,target=/root/.cargo \
    --mount=type=cache,rw,id=target,target=/build/target \
    cargo build --release && cp /build/target/release/chunkah /usr/bin

FROM ${BASE} AS rootfs
ARG DNF_FLAGS
RUN --mount=type=cache,id=dnf,target=/mnt \
    cp -a /mnt /var/cache/libdnf5 && \
    dnf install ${DNF_FLAGS} openssl zlib && rm -rf /var/cache/*
COPY --from=builder /usr/bin/chunkah /usr/bin/chunkah

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
RUN --mount=type=bind,target=/run/src,rw \
    chunkah build --rootfs /rootfs > /run/src/out.ociarchive

FROM ${FINAL_FROM}
ENTRYPOINT ["/usr/bin/chunkah"]
ENV CHUNKAH_ROOTFS=/chunkah
WORKDIR /srv
