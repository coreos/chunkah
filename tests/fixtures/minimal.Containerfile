# This isn't used by any unit tests yet, but I've used the resulting OCI archive
# a few times to debug basic OCI parsing/chunking stuff.
ARG ADD_PACKAGES=""
FROM quay.io/fedora/fedora-minimal AS builder
RUN dnf install -y \
      --installroot /target \
      --use-host-config \
      --nodocs \
      --setopt=install_weak_deps=False \
      filesystem setup $ADD_PACKAGES && rm -rf /var/cache/*

FROM scratch
COPY --from=builder /target /
