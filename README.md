# chunkah

An OCI building tool that takes a flat rootfs and outputs a layered OCI image
with content-based layers.

> [!NOTE]
> chunkah is currently under heavy development and not yet ready for production.
> Experimental usage and feedback is much appreciated!

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

While it's possible to install chunkah as a native CLI tool using `cargo
install`, it's primarily intended to be used as a container image:

```shell
podman run -ti --rm quay.io/jlebon/chunkah --help
```

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
  https://github.com/jlebon/chunkah/releases/download/v0.1.1/Containerfile.splitter
```

Additional arguments can be passed to chunkah using the CHUNKAH_ARGS build
argument.

> [!NOTE]
> You must add the `--skip-unused-stages=false` option (see also [this buildah
> RFE][buildah-rfe]).
>
> For Buildah versions before v1.43, this also requires `-v $(pwd):/run/src
> --security-opt=label=disable`.

<!-- markdownlint-disable-next-line MD028 -->
> [!NOTE]
> There is [a known bug][buildah-annotations-bug] in this workflow preventing
> informational layer annotations added by chunkah from persisting to the final
> image. If you're interested in that information, you must use the `podman
> load` flow below.

Another option is using the chunkah image directly and image mounts:

```shell
IMG=quay.io/fedora/fedora-minimal:latest
podman pull $IMG # image must be available locally
export CHUNKAH_CONFIG_STR="$(podman inspect $IMG)"
podman run --rm --mount=type=image,src=$IMG,dest=/chunkah \
  -e CHUNKAH_CONFIG_STR quay.io/jlebon/chunkah build | podman load
```

#### Using Docker/Moby

You can use the chunkah image directly using image mounts (requires v28+):

```shell
IMG=quay.io/fedora/fedora-minimal:latest
docker pull $IMG # image must be available locally
export CHUNKAH_CONFIG_STR="$(docker inspect $IMG)"
docker run --rm --mount=type=image,src=$IMG,destination=/chunkah \
  -e CHUNKAH_CONFIG_STR quay.io/jlebon/chunkah build > out.ociarchive
docker run --rm -ti -v $(pwd):/srv:z -w /srv quay.io/skopeo/stable \
  copy oci-archive:out.ociarchive docker-archive:out.dockerarchive
docker load -i out.dockerarchive
```

Note the conversion step using `skopeo`; `chunkah` currently only outputs an OCI
archive, which `docker load` does not natively support.

### Splitting an image at build time (buildah/podman only)

This uses a method called the "`FROM oci-archive:` trick", for lack of a better
term. It has the massive advantage of being compatible with a regular `buildah
build` flow and also makes it more natural to apply configs to the image, but is
specific to the Podman ecosystem. This *will not* work with Docker.

```Dockerfile
FROM quay.io/fedora/fedora-minimal:latest AS builder
RUN dnf install -y git-core && dnf clean all

FROM quay.io/jlebon/chunkah AS chunkah
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
> For Buildah versions before v1.43, this also requires `-v $(pwd):/run/src
> --security-opt=label=disable`.

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

It is possible to modify how components are assigned to layers by setting the
`user.component` xattr on files/directories. This can be done using `setfattr`,
e.g.:

```Dockerfile
RUN setfattr -n user.component -v "custom-apps" /usr/bin/my-app
```

This is compatible with rpm-ostree's support for [the same
feature](https://coreos.github.io/rpm-ostree/build-chunked-oci/#assigning-files-to-specific-layers).

### Limiting the number of layers

By default, the maximum number of layers emitted is 64. This can be increased
(up to 448) or decreased using the `--max-layers` option. If the number of
components exceeds the maximum, chunkah will pack multiple components together.
There is thus a tradeoff in deciding this. Fewer layers means losing the
efficiency gains of content-based layers. Too many layers may mean excessive
processing and overhead when pushing/pulling the image.

### Building from a raw rootfs

For completeness, note it's of course also possible to split any arbitrary
rootfs, regardless of where it comes from.

```shell
podman run --rm -v root:/chunkah:z -e CHUNKAH_CONFIG_STR="$(cat config.json)" \
  quay.io/jlebon/chunkah build > out.ociarchive
```

> [!NOTE]
> The `:z` option will relabel all files for access by the container, which may
> be expensive for a large rootfs. You can use `--security-opt=label=disable` to
> avoid this, but it disables SELinux separation with the chunkah container.

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

### Compatibility with bootable (bootc) images

chunkah has no special handling for [bootable container images]. This should
work fine for non-OSTree based images (i.e. "plain" images). Packing still needs
to be fine-tuned for bootable images (or very large images in general). You will
likely want to increase the default maximum number of layers from 64 (e.g. 96)
for better splitting.

OSTree-based images as created by `rpm-ostree` and `ostree container
encapsulate` are not supported.

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
