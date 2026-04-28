---
name: extract-oci-archive
description: Use when you need to extract an OCI archive to inspect its contents, get the rootfs layer, or read the image config.
---

# Extract OCI Archive

## Extract the OCI archive structure

```bash
mkdir /tmp/oci-extracted
tar -xf /path/to/image.ociarchive -C /tmp/oci-extracted
```

## Get the manifest digest from index.json

```bash
MANIFEST=$(jq -r '.manifests[0].digest' /tmp/oci-extracted/index.json \
    | cut -d: -f2)
```

## Extract a layer to get the rootfs

For a single-layer image:

```bash
LAYER=$(jq -r '.layers[0].digest' \
    /tmp/oci-extracted/blobs/sha256/${MANIFEST} | cut -d: -f2)
mkdir /tmp/rootfs
tar -xf /tmp/oci-extracted/blobs/sha256/${LAYER} -C /tmp/rootfs
```

For multi-layer images, extract each layer in order and overlay them.

## Read the image config

The config contains the created timestamp, entrypoint, labels, etc.:

```bash
CONFIG=$(jq -r '.config.digest' \
    /tmp/oci-extracted/blobs/sha256/${MANIFEST} | cut -d: -f2)
jq . /tmp/oci-extracted/blobs/sha256/${CONFIG}
```
