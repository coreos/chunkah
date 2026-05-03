# chunkah

<!-- BEGIN DEV HEADER -->

> [!NOTE]
> This documentation is for the development version of chunkah
> (`quay.io/coreos/chunkah:dev`). For the latest stable release, see
> [README.md](README.md).

<!-- END DEV HEADER -->

An OCI building tool that takes a flat rootfs and outputs a layered OCI image
with content-based layers.

## Table of Contents

- [Motivation](#motivation)
- [Highlights](#highlights)
- [Installation](#installation)
- [Usage](#usage)
  - [Splitting an existing image](#splitting-an-existing-image)
  - [Splitting an image at build time](#splitting-an-image-at-build-time-buildahpodman-only)
- [Advanced Usage](#advanced-usage)
  - [Understanding components](#understanding-components)
  - [Customizing the layers](#customizing-the-layers)
  - [Limiting the number of layers](#limiting-the-number-of-layers)
  - [Building from a raw rootfs](#building-from-a-raw-rootfs)
  - [Customizing the OCI image config and annotations](#customizing-the-oci-image-config-and-annotations)
  - [Pruning and filtering](#pruning-and-filtering)
  - [Architecture](#architecture)
  - [Parallelism](#parallelism)
  - [Compatibility with bootable (bootc) images](#compatibility-with-bootable-bootc-images)
  - [Debugging](#debugging)
- [Relationship to `zstd:chunked`](#relationship-to-zstdchunked)
- [Origins](#origins)

## Motivation

Traditionally, images built using a `Dockerfile` result in a multi-layered image
which model how the `Dockerfile` was written. For example, a separate layer
is created for each `RUN` and `COPY` instructions. This can cause poor layer
caching on clients pulling these images. A single package change may invalidate
a layer much larger than the package itself, requiring re-pulling.

When splitting an image into content-based layers, it doesn't matter how the
final contents of the image were derived. The image is "postprocessed" so that
layers are created in a way that tries to maximize layer reuse. Commonly, this
means grouping together related packages. This has benefits at various levels:
at the network level (common layers do not need to be re-pulled), at the storage
level (common layers are stored once), and at the runtime level (e.g. libraries
are only mapped once).

chunkah allows you to keep building your image as you currently do, and then
perform this content-based layer splitting.

## Highlights

- **Content agnostic** — Compatible with RPM-based images, but not only. Other
  package ecosystems can be supported, as well as fully custom content.
- **Container-native** — Best used as a container image, either as part of a
  multi-staged build, or standalone.
- **Zero diff** — Apart from modification time, content is never modified.
- **Reproducible** — Supports `SOURCE_DATE_EPOCH` for reproducible layers.

It is a non-goal to support initial building of the root filesystem itself.
Lots of tools for that exist already. It is also currently a non-goal to
preprocess the rootfs to remove common sources of non-reproducibility (such as
[add-determinism]). This can be done by the image build process itself.

## Installation

chunkah is primarily intended to be used as a container image:

```shell
podman run -ti --rm quay.io/coreos/chunkah --help
```

However, if you're currently building images using a multi-stage build, it
may be more convenient to `cargo install` the binary into your builder image
(whether at runtime or build time if you own the builder image). chunkah is also
packaged in Fedora, making it easier to do this there.

## Usage

There are two main ways to use chunkah:

- splitting an existing image
- splitting an image at build time

### Splitting an existing image

#### Using Podman/Buildah

When using Podman/Buildah, the most natural way to split an existing image is to
use the `Containerfile.splitter`, passing the target image as the `--from`:

```shell
IMG=quay.io/fedora/fedora-minimal:latest
buildah build --skip-unused-stages=false --from $IMG \
  --build-arg CHUNKAH_CONFIG_STR="$(podman inspect $IMG)" \
  https://github.com/coreos/chunkah/releases/download/v0.4.0/Containerfile.splitter
```

Additional arguments can be passed to chunkah using the CHUNKAH_ARGS build
argument.

> [!NOTE]
> You must add the `--skip-unused-stages=false` option (see also [this buildah
> RFE][buildah-rfe]).
>
> For Buildah versions before v1.44, this also requires `-v $(pwd):/run/src
> --security-opt=label=disable`.

Another option is using the chunkah image directly and image mounts:

```shell
IMG=quay.io/fedora/fedora-minimal:latest
podman pull $IMG # image must be available locally
export CHUNKAH_CONFIG_STR="$(podman inspect $IMG)"
podman run --rm --mount=type=image,src=$IMG,dest=/chunkah \
  -e CHUNKAH_CONFIG_STR quay.io/coreos/chunkah build \
    -t localhost/fedora-minimal-chunked:latest | podman load
```

The `-t`/`--tag` option sets the image name in the OCI archive so that `podman
load` automatically tags the loaded image. Without it, the image is loaded as an
unnamed image identified only by its digest.

#### Using Docker/Moby

You can use the chunkah image directly using image mounts (requires v28+):

```shell
IMG=quay.io/fedora/fedora-minimal:latest
docker pull $IMG # image must be available locally
export CHUNKAH_CONFIG_STR="$(docker inspect $IMG)"
docker run --rm --mount=type=image,src=$IMG,destination=/chunkah \
  -e CHUNKAH_CONFIG_STR quay.io/coreos/chunkah build \
    -t localhost/fedora-minimal-chunked:latest | docker load
```

Note `docker load` support for OCI archives requires the [containerd image
store] (default on new installations starting from v29+). If using the legacy
graph driver, instead of piping directly into `docker load` as above, you can
redirect to a file to save the OCI archive, and then use skopeo to convert to
the Docker archive format:

```shell
docker run --rm -ti -v $(pwd):/srv:z -w /srv quay.io/skopeo/stable \
  copy oci-archive:out.ociarchive docker-archive:out.dockerarchive:chunked
docker load -i out.dockerarchive
```

[containerd image store]: https://docs.docker.com/engine/storage/containerd/

### Splitting an image at build time (buildah/podman only)

This uses a method called the "`FROM oci-archive:` trick", for lack of a better
term. It has the massive advantage of being compatible with a regular `buildah
build` flow and also makes it more natural to apply configs to the image, but is
specific to the Podman ecosystem. This *will not* work with Docker.

```Dockerfile
# Optional; by default base image metadata (like labels) are lost and need to
# either be repeated in the final stage or passed in via a build arg like this
# one. Use with `--build-arg CHUNKAH_CONFIG_STR=$(podman inspect $IMG)`.
ARG CHUNKAH_CONFIG_STR

FROM quay.io/fedora/fedora-minimal:latest AS builder
RUN dnf install -y git-core && dnf clean all

FROM quay.io/coreos/chunkah AS chunkah
ARG CHUNKAH_CONFIG_STR
RUN --mount=from=builder,src=/,target=/chunkah,ro \
    --mount=type=bind,target=/run/src,rw \
        chunkah build > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
ENTRYPOINT ["git"]
```

> [!NOTE]
> When building your image, you must also add the `--skip-unused-stages=false`
> option (see also [this buildah RFE][buildah-rfe]), and you cannot use the
> `--jobs` option.
>
> For Buildah versions before v1.44, this also requires `-v $(pwd):/run/src
> --security-opt=label=disable`.

<!-- markdownlint-disable-next-line MD028 -->
> [!NOTE]
> There is [a known bug][buildah-annotations-bug] in this workflow preventing
> informational layer annotations added by chunkah from persisting to the
> final image when additional instructions follow the final `FROM`. If you're
> interested in that information, you must either use `--config-str` instead
> to pass your config, or run chunkah as a separate step as described in the
> previous section.

## Advanced Usage

### Understanding components

A component is a logical grouping of files that belong together. For example,
all files from an RPM belong to the same component. Layers are created based on
found components.

A component repo is a source of data from which components can be created. For
example, the rpmdb is a component repo (one can imagine similar component repos
for other distros). There is also an xattr-based component repo (see the section
"Customizing the layers" below). Multiple component repos can be active at once.

### Customizing the layers

It is possible to create custom components by setting the `user.component` xattr
on files/directories. This can be done using `setfattr`, e.g.:

```Dockerfile
RUN setfattr -n user.component -v "custom-apps" /usr/bin/my-app
```

This is compatible with rpm-ostree's support for [the same
feature](https://coreos.github.io/rpm-ostree/build-chunked-oci/#assigning-files-to-specific-layers).
However, unlike rpm-ostree, this does not guarantee unique layers per xattr
component.

In addition, the `user.update-interval` xattr is also supported. The value is
the average number of days between updates, either as an integer or a named
label (`daily`, `weekly`, `biweekly`, `monthly`, `quarterly`, `yearly`), e.g.:

```Dockerfile
RUN setfattr -n user.component -v "custom-apps" /usr/bin/my-app && \
    setfattr -n user.update-interval -v "monthly" /usr/bin/my-app
```

This only needs to be set on one of the files in the component. If multiple
files have conflicting values, chunkah reports an error.

It is strongly recommended to set this xattr. A rough approximation is fine.
This helps the packing algorithm make better decisions about which components to
group together. When missing, defaults to `weekly`.

### Limiting the number of layers

By default, the maximum number of layers emitted is 64. This can be increased or
decreased using the `--max-layers` option. If the number of components exceeds
the maximum, chunkah will pack multiple components together. There is thus a
tradeoff in deciding this. Fewer layers means losing the efficiency gains of
content-based layers. Too many layers may mean excessive processing and overhead
when pushing/pulling the image. Note that containers-storage has a hard limit of
500 layers.

### Building from a raw rootfs

For completeness, note it's of course also possible to split any arbitrary
rootfs, regardless of where it comes from:

```shell
podman run --rm -v /path/to/rootfs:/chunkah:z \
  -e CHUNKAH_CONFIG_STR="$(cat config.json)" \
  quay.io/coreos/chunkah build > out.ociarchive
```

> [!NOTE]
> The `:z` option will relabel all files for access by the container, which may
> be expensive for a large rootfs. You can use `--security-opt=label=disable` to
> avoid this, but it disables SELinux separation with the chunkah container.

When running chunkah directly in this way, the OCI archive is written to stdout
by default. Use `-o`/`--output` to write to a file instead (whose directory
would then have to be mounted in).

By default, layers and the OCI archive are uncompressed. Use `--compressed`
to enable gzip compression for both. The compression level can be tuned with
`--compression-level` (0-9, default 6).

### Customizing the OCI image config and annotations

The OCI image config can be provided via the `--config` option (as a file) or
`--config-str`/`CHUNKAH_CONFIG_STR` (inline). The primary format is the [OCI
image config] spec as JSON:

```json
{
    "Entrypoint": ["/bin/bash"],
    "Cmd": ["-c", "echo hi"],
    "WorkDir": "/root"
}
```

The output format of `podman inspect` and `docker inspect` are also supported,
mostly for convenience when splitting an existing image, though it does also
have the advantage of capturing annotations. Otherwise, it's also possible to
set annotations directly using `--annotation`. Labels can also be added via
`--label`.

### Pruning and filtering

The `--prune` option excludes paths from the rootfs. It can be specified
multiple times. The trailing slash matters:

- `--prune /path` excludes the directory and all its descendants entirely.
- `--prune /path/` excludes only the contents but keeps the directory itself.

By default, chunkah errors when encountering special file types (sockets,
FIFOs, block/char devices). Use `--skip-special-files` to silently skip them
instead.

### Architecture

The `--arch` option overrides the target architecture for the output image. This
is useful when splitting an image whose architecture differs from the running
host. If not provided, the architecture from the config is used (if available),
or the current system architecture otherwise. Common aliases are supported (e.g.
`x86_64` maps to `amd64`, `aarch64` to `arm64`).

### Parallelism

Layers are written in parallel. The number of threads can be controlled with
`-T`/`--threads` (or `CHUNKAH_THREADS`). By default, the number of available
CPUs is used.

### Compatibility with bootable (bootc) images

chunkah has no special handling for [bootable container images]. This should
work fine for non-OSTree based images (i.e. "plain" images). Packing still needs
to be fine-tuned for bootable images (or very large images in general). You will
likely want to increase the default maximum number of layers from 64 (e.g. 128)
for better splitting.

As mentioned in [this
section](#splitting-an-image-at-build-time-buildahpodman-only), in the build
time flow, labels from the base image will be lost, including versioning
information and `containers.bootc=1`, which is required by bootc. So you'll want
to use a `CHUNKAH_CONFIG_STR` build arg or just re-add the label.

Using chunkah to rechunk an OSTree-based bootc image is also possible by
transforming it into a plain one by passing `--prune /sysroot/` to strip OSTree
data from the image. If base metadata is persisted (either the existing image
flow, or the build time flow with inspect output passed in as a build arg), you
will need to remove the ostree-related labels using the `--label KEY-` option:

```Dockerfile
ARG CHUNKAH_CONFIG_STR

FROM quay.io/fedora/fedora-bootc:latest AS builder
RUN dnf install -y tmux && dnf clean all
RUN bootc container lint

FROM quay.io/coreos/chunkah AS chunkah
ARG CHUNKAH_CONFIG_STR
RUN --mount=from=builder,src=/,target=/chunkah,ro \
    --mount=type=bind,target=/run/src,rw \
        chunkah build --prune /sysroot/ --max-layers 128 \
          --label ostree.commit- --label ostree.final-diffid- \
          > /run/src/out.ociarchive

FROM oci-archive:out.ociarchive
```

### Debugging

Use `-v` for verbose output or the `RUST_LOG` environment variable for
fine-grained control (e.g. `RUST_LOG=chunkah=debug`). Logs are written
to stderr. (There is also `-vv` for trace output mostly meant for chunkah
development. You'll want to redirect stderr to a file!)

## Relationship to `zstd:chunked`

[zstd:chunked] is a [container-libs] feature that enables partial layer pulls,
fetching only changed files and chunks within a layer via HTTP range requests.
chunkah and zstd:chunked are complementary:

- chunkah operates at build time: it structures the image so that unchanged
  content lives in unchanged layers, maximizing layer-level deduplication. This
  works with any OCI registry and requires no special client support.
- zstd:chunked operates at pull time: within layers, only the files (technically
  chunks *within* files) that changed are fetched. This requires client support
  and HTTP range requests from the registry, and has higher CPU and memory
  overhead on the client side.

Used together, most layers are reused at the registry level (thanks to chunkah),
and for the few that *did* change, you can efficiently pull just those (thanks
to zstd:chunked), minimizing overhead.

## Origins

chunkah is a generalized successor to rpm-ostree's [build-chunked-oci] command
which does content-based layer splitting on RPM-based [bootable container
images]. Unlike rpm-ostree, chunkah is not tied to bootable containers nor RPMs.
The name is a nod to this ancestry and to buildah, with which it integrates
well.

[add-determinism]: https://github.com/keszybz/add-determinism
[bootable container images]: https://containers.github.io/bootable/
[build-chunked-oci]: https://coreos.github.io/rpm-ostree/build-chunked-oci/
[OCI image config]: https://github.com/opencontainers/image-spec/blob/26647a49f642c7d22a1cd3aa0a48e4650a542269/specs-go/v1/config.go#L24
[buildah-rfe]: https://github.com/containers/buildah/issues/6621
[buildah-annotations-bug]: https://github.com/containers/buildah/issues/6652
[zstd:chunked]: https://github.com/containers/container-libs/blob/main/storage/docs/containers-storage-zstd-chunked.md
[container-libs]: https://github.com/containers/container-libs
