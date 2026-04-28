---
name: benchmark-packing
description: Benchmark chunkah packing algorithm changes against a series of OCI images.
---

# Benchmark Packing

## Overview

This skill guides benchmarking of packing algorithm changes by
chunking a series of consecutive OCI images and measuring layer
reuse between them. The key tools are:

- `tools/chunk-image-series.py` -- chunks a series of images
- `tools/analyze-layer-reuse.py` -- compares layer sharing across
  the chunked series

## Pipeline

### 1. Build chunkah

After making code changes, build the container image:

```bash
just buildimg --no-chunk
```

This produces `localhost/chunkah:latest`.

### 2. Prepare source images

Source images must be in containers-storage. Either:

**From OCI archives:**

```bash
for archive in /path/to/*.ociarchive; do
    version=$(echo "${archive}" | grep -oP 'PATTERN')
    skopeo copy "oci-archive:${archive}" "containers-storage:localhost/myrepo:${version}"
done
```

**From a registry:**

`chunk-image-series.py` can pull directly from a registry:

```bash
tools/chunk-image-series.py docker://quay.io/fedora/fedora-coreos \
    --tag-filter '43.*' --limit 10 ...
```

### 3. Chunk the series

```bash
tools/chunk-image-series.py containers-storage:localhost/myrepo \
    --tag-filter '43.*' \
    --prefix myrepo-chunked \
    --chunkah-image localhost/chunkah \
    --force \
    -- --prune /sysroot --max-layers 128
```

Key flags:

- `--force` to overwrite results from prior runs
- `--limit N` to control how many images to process
- Extra chunkah args go after `--`

### 4. Analyze layer reuse

Pass all chunked images as separate positional arguments:

```bash
image_args=()
for i in $(seq 0 9); do
    image_args+=("containers-storage:localhost/myrepo-chunked:${i}")
done
tools/analyze-layer-reuse.py --json "${image_args[@]}" > results.json
```

IMPORTANT: do NOT use brace expansion inside quotes (e.g.
`"...:{0,1,2}"`) -- it won't expand. Always build the argument
list explicitly.

The JSON summary fields are:

- `avg_reuse_ratio` -- fraction of data shared (0.0 to 1.0)
- `avg_download_bytes` -- average new data per update

### 5. Clean up chunked images

Always clean up chunked images after capturing results:

```bash
for i in $(seq 0 9); do
    podman rmi "localhost/myrepo-chunked:${i}" 2>/dev/null || true
done
```

### 6. Compare against original packing

Always measure the original (un-chunked) images as a baseline:

```bash
image_args=()
for tag in $(podman images --filter reference='localhost/myrepo' \
    --format '{{.Tag}}' | sort); do
    image_args+=("containers-storage:localhost/myrepo:${tag}")
done
tools/analyze-layer-reuse.py --json "${image_args[@]}" > original.json
```

## Comparing multiple configurations

To compare a code change against the current defaults:

1. Chunk with the baseline chunkah (`quay.io/coreos/chunkah:dev`)
2. Capture and save JSON results
3. Clean up chunked images
4. Build local chunkah with your changes
5. Chunk with `localhost/chunkah`
6. Capture and save JSON results
7. Clean up chunked images
8. Compare the two JSON files

The source images and original baseline can be reused across runs.

## Pitfalls

### Duplicate tags

`chunk-image-series.py` creates temporary `localhost/tmp-chunk-src`
tags during chunking. These are cleaned up on exit, but if a prior
run was interrupted, stale tags may remain and cause duplicate
entries. Before running analysis, verify tags are clean:

```bash
podman images --filter reference='localhost/myrepo' --format '{{.Tag}}' | sort
```

If you see duplicate tags, remove them before proceeding. A red
flag in results is `min_download_bytes: 0` -- this means two
consecutive images were identical (likely duplicates).

### Sample size sensitivity

Results can vary significantly depending on the specific time
window of builds tested. Always test against multiple independent
corpora (e.g. different image types, different time periods) before
concluding a change is beneficial. An improvement on one sample may
not generalize.

### Layer count matters

The default `--max-layers` is 64. Results at 64 layers vs 128
layers can differ substantially. Always compare configurations at
the same layer count.
