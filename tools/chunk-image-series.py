#!/usr/bin/env python3
"""Pull images from a registry, run chunkah on each, store results.

Example usage for FCOS:

    ./tools/chunk-image-series.py quay.io/fedora/fedora-coreos \\
        --tag-filter '*.*.3.*' --limit 5 --keep-originals \\
        --prefix fedora-coreos-test \\
        --chunkah-image localhost/chunkah \\
        -- --compressed --label ostree.commit- --label ostree.final-diffid- --prune /sysroot/

This pulls the 5 most recent stable FCOS images (matching *.*.3.*), runs
chunkah on each, and stores the results as localhost/fedora-coreos-test:0-4.
The originals are kept as localhost/fedora-coreos-test-orig:0-4 for comparison.
"""

import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys
import tempfile


def main():
    args = parse_args()
    run_chunking(args)


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Pull images from a registry, run chunkah on each, store results",
        usage="%(prog)s [OPTIONS] REPO [-- CHUNKAH_ARGS...]",
    )
    parser.add_argument("repo", help="OCI image repository (e.g., quay.io/fedora/fedora-coreos)")
    parser.add_argument(
        "--tag-filter",
        default="*",
        help="Glob pattern for tag filtering (default: *)",
    )
    parser.add_argument(
        "--sort-by",
        choices=["version", "name", "date"],
        default="version",
        help="How to sort tags: version (natural sort), name (alphabetical), "
             "or date (by image creation time) (default: version)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        help="Maximum number of images to process (takes the N most recent)",
    )
    parser.add_argument(
        "--prefix",
        help="Storage prefix (default: derived from repo name)",
    )
    parser.add_argument(
        "--chunkah-image",
        default="quay.io/jlebon/chunkah",
        help="Chunkah image to use (default: quay.io/jlebon/chunkah)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be processed without doing it",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Overwrite existing images at prefix",
    )
    parser.add_argument(
        "--keep-originals",
        action="store_true",
        help="Also store original (un-chunked) images as {prefix}-orig:N",
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="Verbose output",
    )
    parser.add_argument(
        "chunkah_args",
        nargs="*",
        help="Additional arguments to pass to chunkah (after --)",
    )
    args = parser.parse_args()

    # Derive prefix from repo name if not specified
    if args.prefix is None:
        # e.g., quay.io/fedora/fedora-coreos -> fedora-coreos-chunked
        repo_name = args.repo.split("/")[-1]
        args.prefix = f"{repo_name}-chunked"

    return args


def run_chunking(args: argparse.Namespace):
    """Run the chunking pipeline."""
    try:
        tags = get_sorted_tags(args)

        if args.dry_run:
            step("Dry run - would process:")
            for i, tag in enumerate(tags):
                print(f"  {args.repo}:{tag} -> localhost/{args.prefix}:{i}")
                if args.keep_originals:
                    print(f"    (original -> localhost/{args.prefix}-orig:{i})")
            if args.chunkah_args:
                print(f"\nChunkah extra args: {' '.join(args.chunkah_args)}")
            return

        # Check for existing images
        if not args.force:
            existing = _find_existing_images(args.prefix)
            if args.keep_originals:
                existing.extend(_find_existing_images(f"{args.prefix}-orig"))
            if existing:
                die(f"Images already exist at prefix '{args.prefix}': {existing}\n"
                    f"Use --force to overwrite")

        # Process each tag
        processed = []
        failed = []
        temp_refs = []

        for i, tag in enumerate(tags):
            result = process_tag(args, i, len(tags), tag, temp_refs)
            if result is not None:
                processed.append(result)
            else:
                failed.append(tag)

        # Cleanup temp images
        if temp_refs:
            step("Cleaning up temporary images...")
            cleanup_temp_images(temp_refs)

        print_summary(processed, failed, args.keep_originals)

    except subprocess.CalledProcessError as e:
        die(f"Command failed: {e.cmd}")
    except Exception as e:
        die(str(e))


def process_tag(args: argparse.Namespace, i: int, total: int, tag: str,
                temp_refs: list[str]) -> tuple[int, str, str, str | None] | None:
    """Process a single tag: pull, chunk, and load.

    Returns tuple of (index, tag, target_ref, orig_ref) on success, None on failure.
    Appends source_ref to temp_refs for later cleanup.
    """
    step(f"Processing tag {i + 1}/{total}: {tag}")
    source_ref = f"localhost/tmp-chunk-src:{tag}"
    target_ref = f"localhost/{args.prefix}:{i}"
    orig_ref = f"localhost/{args.prefix}-orig:{i}" if args.keep_originals else None

    try:
        print("  Pulling image...")
        pull_image(args.repo, tag, source_ref, verbose=args.verbose)

        # Keep original if requested (tag it before chunking)
        if orig_ref:
            print(f"  Storing original as {orig_ref}...")
            run("podman", "tag", source_ref, orig_ref)

        temp_refs.append(source_ref)

        print("  Running chunkah...")
        ociarchive = chunk_image(source_ref, args.chunkah_image, tag,
                                 chunkah_args=args.chunkah_args,
                                 verbose=args.verbose)

        print(f"  Loading as {target_ref}...")
        load_chunked_image(ociarchive, target_ref)
        os.unlink(ociarchive)

        print(f"  Done: {target_ref}")
        return (i, tag, target_ref, orig_ref)

    except subprocess.CalledProcessError as e:
        print(f"  ERROR: Failed to process {tag}: {e}", file=sys.stderr)
        return None


def print_summary(processed: list[tuple], failed: list[str], keep_originals: bool):
    """Print final summary of processed and failed tags."""
    print()
    if processed:
        step("Done! Chunked images stored as:")
        for i, tag, ref, orig_ref in processed:
            print(f"  {ref}  ({tag})")
        if keep_originals:
            step("Original images stored as:")
            for i, tag, ref, orig_ref in processed:
                if orig_ref:
                    print(f"  {orig_ref}  ({tag})")

    if failed:
        print()
        step("Failed to process:")
        for tag in failed:
            print(f"  {tag}")
        sys.exit(1)


def get_sorted_tags(args: argparse.Namespace) -> list[str]:
    """List, filter, sort, and limit tags from remote registry."""
    step(f"Listing tags from {args.repo} matching '{args.tag_filter}'...")
    tags = _list_tags(args.repo, args.tag_filter)

    if not tags:
        die(f"No tags found matching '{args.tag_filter}'")

    if args.sort_by == "date":
        step("Fetching image creation dates (this may take a while)...")
        tags = _sort_tags_by_date(args.repo, tags, verbose=args.verbose)
    else:
        tags = _sort_tags(tags, args.sort_by)

    if args.limit:
        # Take the N most recent (from the end of sorted list)
        tags = tags[-args.limit:]

    print(f"Found {len(tags)} matching tags")
    if args.verbose:
        for tag in tags:
            print(f"  {tag}")

    return tags


def _list_tags(repo: str, pattern: str) -> list[str]:
    """List tags from remote registry matching glob pattern."""
    output = run_output("skopeo", "list-tags", f"docker://{repo}")
    data = json.loads(output)
    all_tags = data.get("Tags", [])
    return [tag for tag in all_tags if _match_glob(tag, pattern)]


def _sort_tags(tags: list[str], sort_by: str) -> list[str]:
    """Sort tags by version (natural sort) or name (alphabetical)."""
    if sort_by == "name":
        return sorted(tags)
    else:
        return sorted(tags, key=_natural_sort_key)


def _sort_tags_by_date(repo: str, tags: list[str], verbose: bool = False) -> list[str]:
    """Sort tags by image creation date (oldest first)."""
    tag_dates = []
    for tag in tags:
        if verbose:
            print(f"  Inspecting {tag}...")
        try:
            created = _get_image_created(repo, tag)
            tag_dates.append((tag, created))
        except Exception as e:
            print(f"  Warning: Could not get date for {tag}: {e}", file=sys.stderr)
            # Use empty string so it sorts first (oldest)
            tag_dates.append((tag, ""))

    # Sort by date (ISO 8601 format sorts lexicographically)
    tag_dates.sort(key=lambda x: x[1])
    return [tag for tag, _ in tag_dates]


def pull_image(repo: str, tag: str, target_ref: str, verbose: bool = False):
    """Pull image to containers-storage."""
    cmd = [
        "skopeo", "copy",
        f"docker://{repo}:{tag}",
        f"containers-storage:{target_ref}",
    ]
    if verbose:
        print(f"    Running: {' '.join(cmd)}")
    run(*cmd)


def chunk_image(source_ref: str, chunkah_image: str, original_tag: str,
                chunkah_args: list[str] | None = None,
                verbose: bool = False) -> str:
    """Run chunkah on image, return path to OCI archive."""
    # Get image config
    config_str = _get_image_config(source_ref)

    # Create temp file for output
    fd, ociarchive = tempfile.mkstemp(suffix=".ociarchive")
    os.close(fd)

    try:
        cmd = [
            "podman", "run", "--rm",
            f"--mount=type=image,src={source_ref},target=/chunkah",
            "-e", f"CHUNKAH_CONFIG_STR={config_str}",
            chunkah_image,
            "build",
            "--label", f"org.chunkah.original-tag={original_tag}",
        ]
        if chunkah_args:
            cmd.extend(chunkah_args)
        if verbose:
            extra_args = ["--label", f"org.chunkah.original-tag={original_tag}"]
            if chunkah_args:
                extra_args.extend(chunkah_args)
            print(f"    Running: podman run --rm --mount=... {chunkah_image} build {' '.join(extra_args)}")

        with open(ociarchive, "wb") as f:
            subprocess.check_call(cmd, stdout=f)

        return ociarchive
    except Exception:
        os.unlink(ociarchive)
        raise


def load_chunked_image(ociarchive: str, target_ref: str):
    """Load OCI archive into containers-storage with target tag."""
    # Load the image and capture the ID
    output = run_output("podman", "load", "-i", ociarchive)
    # Output is like "Loaded image: sha256:abc123..."
    image_id = output.strip().split()[-1]
    if image_id.startswith("sha256:"):
        image_id = image_id[7:]

    # Tag it with our target reference
    run("podman", "tag", image_id, target_ref)


def cleanup_temp_images(refs: list[str]):
    """Remove temporary source images from containers-storage."""
    for ref in refs:
        try:
            subprocess.run(
                ["podman", "rmi", "-f", ref],
                capture_output=True,
                check=False,
            )
        except Exception:
            pass  # Ignore cleanup errors


def _match_glob(tag: str, pattern: str) -> bool:
    """Check if tag matches glob pattern."""
    return fnmatch.fnmatch(tag, pattern)


def _natural_sort_key(s: str) -> list:
    """Generate sort key for natural/version sorting.

    Splits string into numeric and non-numeric parts for proper ordering.
    e.g., 'stable-41.20240101' < 'stable-41.20240201' < 'stable-42.20240101'
    """
    parts = re.split(r"(\d+)", s)
    return [int(p) if p.isdigit() else p.lower() for p in parts]


def _get_image_config(image_ref: str) -> str:
    """Get podman inspect output for chunkah config."""
    return run_output("podman", "inspect", image_ref)


def _get_image_created(repo: str, tag: str) -> str:
    """Get image creation date from remote registry."""
    output = run_output("skopeo", "inspect", f"docker://{repo}:{tag}")
    data = json.loads(output)
    return data.get("Created", "")


def _find_existing_images(prefix: str) -> list[str]:
    """Find existing images with the given prefix."""
    try:
        output = run_output(
            "podman", "images", "--format", "{{.Repository}}:{{.Tag}}",
            f"localhost/{prefix}"
        )
        return [line.strip() for line in output.strip().split("\n") if line.strip()]
    except subprocess.CalledProcessError:
        return []


def step(msg: str):
    print(f"==> {msg}")


def die(msg: str):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(1)


def run(*args: str):
    """Run a command."""
    subprocess.check_call(args)


def run_output(*args: str) -> str:
    """Run a command and return its stdout."""
    return subprocess.check_output(args, text=True)


if __name__ == "__main__":
    main()
