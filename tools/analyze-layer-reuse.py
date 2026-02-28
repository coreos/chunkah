#!/usr/bin/env python3
"""Compare images and report layer sharing statistics.

Example usage:

    ./tools/analyze-layer-reuse.py containers-storage:localhost/img:{a,b,c}

Image references must be transport-qualified (e.g. containers-storage:,
oci-archive:, docker://).
"""

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass


@dataclass
class LayerInfo:
    """Information about a single layer."""
    digest: str
    size: int  # Compressed size (download size) from LayersData[].Size
    component: str | None  # From LayersData[].Annotations["org.chunkah.component"]


@dataclass
class ImageInfo:
    """Information about an image's layers."""
    ref: str
    created: str | None  # Image creation timestamp from skopeo inspect
    layers: list[LayerInfo]
    total_size: int


@dataclass
class UpdateAnalysis:
    """Analysis of layer changes between two images."""
    from_ref: str
    to_ref: str
    from_created: str | None
    to_created: str | None
    shared_layers: list[LayerInfo]
    added_layers: list[LayerInfo]
    removed_layers: list[LayerInfo]
    shared_bytes: int
    download_bytes: int


def main():
    parser = argparse.ArgumentParser(
        description="Compare sequential images and report layer sharing statistics",
    )
    parser.add_argument(
        "images",
        nargs="+",
        metavar="IMAGE",
        help="Transport-qualified image references to compare",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        dest="json_output",
        help="Output as JSON",
    )
    parser.add_argument(
        "--show-changed-components",
        action="store_true",
        help="Show which components changed in each update",
    )
    parser.add_argument(
        "--show-unchanged-components",
        action="store_true",
        help="Show which components were unchanged (shared) in each update",
    )
    args = parser.parse_args()

    if len(args.images) < 2:
        die("Need at least 2 images for update analysis")

    try:
        # Get info for each image
        images = []
        for ref in args.images:
            images.append(get_image_info(ref))

        # If component display requested and annotations are missing, try history fallback
        if args.show_changed_components or args.show_unchanged_components:
            _backfill_components_from_history(images)

        # Analyze sequential updates
        analyses = []
        for i in range(len(images) - 1):
            analyses.append(analyze_update(images[i], images[i + 1]))

        # Output results
        if args.json_output:
            print(format_json_output(images, analyses))
        else:
            print(format_human_output(images, analyses,
                                      args.show_changed_components,
                                      args.show_unchanged_components))

    except subprocess.CalledProcessError as e:
        die(f"Command failed: {e.cmd}")
    except Exception as e:
        die(str(e))


def get_image_info(image_ref: str) -> ImageInfo:
    """Get layer information for an image via skopeo.

    The image_ref must be transport-qualified
    (e.g., containers-storage:localhost/myimage:tag).
    """
    output = run_output("skopeo", "inspect", image_ref)
    data = json.loads(output)

    layers = []
    total_size = 0

    for layer_data in data.get("LayersData", []):
        digest = layer_data.get("Digest", "")
        size = layer_data.get("Size", 0)
        annotations = layer_data.get("Annotations", {}) or {}
        component = annotations.get("org.chunkah.component")

        layers.append(LayerInfo(digest=digest, size=size, component=component))
        total_size += size

    # Get creation date (truncate to date only)
    created_raw = data.get("Created", "")
    created = created_raw[:10] if created_raw else None

    return ImageInfo(
        ref=image_ref,
        created=created,
        layers=layers,
        total_size=total_size,
    )


def _backfill_components_from_history(images: list[ImageInfo]):
    """Backfill component names from OCI history if annotations are missing.

    This is a fallback for images that only have history metadata (like those
    built with older chunkah versions). See inspect-layers.sh for the same
    approach.
    """
    for img in images:
        if any(layer.component for layer in img.layers):
            continue

        config_output = run_output("skopeo", "inspect", "--config", img.ref)
        config = json.loads(config_output)
        history = config.get("history", []) or []

        if not any(entry.get("author") == "chunkah" for entry in history):
            continue

        _warn_history_fallback()
        component_names = [
            entry.get("comment", "unknown")
            for entry in history
            if not entry.get("empty_layer", False)
        ]
        if len(component_names) != len(img.layers):
            die(f"{img.ref}: history has {len(component_names)} non-empty "
                f"entries but image has {len(img.layers)} layers")
        for i, name in enumerate(component_names):
            img.layers[i].component = name


def analyze_update(from_img: ImageInfo, to_img: ImageInfo) -> UpdateAnalysis:
    """Compare two images and calculate layer differences."""
    from_digests = {layer.digest: layer for layer in from_img.layers}
    to_digests = {layer.digest: layer for layer in to_img.layers}

    shared_layers = []
    added_layers = []
    removed_layers = []

    # Find shared and added layers
    for digest, layer in to_digests.items():
        if digest in from_digests:
            shared_layers.append(layer)
        else:
            added_layers.append(layer)

    # Find removed layers
    for digest, layer in from_digests.items():
        if digest not in to_digests:
            removed_layers.append(layer)

    shared_bytes = sum(layer.size for layer in shared_layers)
    download_bytes = sum(layer.size for layer in added_layers)

    return UpdateAnalysis(
        from_ref=from_img.ref,
        to_ref=to_img.ref,
        from_created=from_img.created,
        to_created=to_img.created,
        shared_layers=shared_layers,
        added_layers=added_layers,
        removed_layers=removed_layers,
        shared_bytes=shared_bytes,
        download_bytes=download_bytes,
    )


def format_human_output(images: list[ImageInfo], analyses: list[UpdateAnalysis],
                        show_changed_components: bool,
                        show_unchanged_components: bool) -> str:
    """Format analysis results for human consumption."""
    lines = []

    # Image summary
    lines.append("==> Image Summary:")
    for img in images:
        created = f"{img.created}  " if img.created else ""
        size_str = _format_bytes(img.total_size)
        lines.append(f"    {created}{img.ref}  {len(img.layers)} layers, {size_str}")
    lines.append("")

    # Update analysis
    if analyses:
        lines.append("==> Update Analysis:")
        lines.append("")

        for analysis in analyses:
            total_bytes = analysis.shared_bytes + analysis.download_bytes
            reuse_ratio = analysis.shared_bytes / total_bytes if total_bytes > 0 else 0
            shared_n = len(analysis.shared_layers)
            total_n = len(analysis.shared_layers) + len(analysis.added_layers)

            from_created = f"  ({analysis.from_created})" if analysis.from_created else ""
            to_created = f"  ({analysis.to_created})" if analysis.to_created else ""
            lines.append(f"    From: {analysis.from_ref}{from_created}")
            lines.append(f"    To:   {analysis.to_ref}{to_created}")
            lines.append(f"      Shared:   {len(analysis.shared_layers):3} layers ({_format_bytes(analysis.shared_bytes)})")
            lines.append(f"      Added:    {len(analysis.added_layers):3} layers ({_format_bytes(analysis.download_bytes)} download)")
            lines.append(f"      Removed:  {len(analysis.removed_layers):3} layers")
            lines.append(f"      Data reuse: {reuse_ratio * 100:.1f}% ({shared_n}/{total_n} layers)")

            if show_changed_components and analysis.added_layers:
                changed = _components_by_size(analysis.added_layers)
                if changed:
                    rest = f", ... and {len(changed) - 5} more" if len(changed) > 5 else ""
                    lines.append(f"      Changed:   {', '.join(changed[:5])}{rest}")

            if show_unchanged_components and analysis.shared_layers:
                unchanged = _components_by_size(analysis.shared_layers)
                if unchanged:
                    rest = f", ... and {len(unchanged) - 5} more" if len(unchanged) > 5 else ""
                    lines.append(f"      Unchanged: {', '.join(unchanged[:5])}{rest}")

            lines.append("")

    # Summary statistics
    if analyses:
        summary = _calculate_summary(analyses)
        lines.append("==> Summary:")
        lines.append(f"    Total updates analyzed: {summary['update_count']}")
        lines.append(f"    Average data reuse:    {summary['avg_reuse_ratio'] * 100:.1f}%")
        lines.append(f"    Average download size:  {_format_bytes(summary['avg_download_bytes'])}")

        if summary['update_count'] > 1:
            lines.append(f"    Min download:           {_format_bytes(summary['min_download_bytes'])}")
            lines.append(f"    Max download:           {_format_bytes(summary['max_download_bytes'])}")
        lines.append("")

    return "\n".join(lines)


def format_json_output(images: list[ImageInfo],
                       analyses: list[UpdateAnalysis]) -> str:
    """Format analysis results as JSON."""

    def _format_image(img: ImageInfo) -> dict:
        return {
            "ref": img.ref,
            "created": img.created,
            "layer_count": len(img.layers),
            "total_bytes": img.total_size,
            "layers": [
                {
                    "digest": layer.digest,
                    "size": layer.size,
                    "component": layer.component,
                }
                for layer in img.layers
            ],
        }

    def _format_analysis(analysis: UpdateAnalysis) -> dict:
        total_bytes = analysis.shared_bytes + analysis.download_bytes
        return {
            "from": analysis.from_ref,
            "from_created": analysis.from_created,
            "to": analysis.to_ref,
            "to_created": analysis.to_created,
            "shared_layer_count": len(analysis.shared_layers),
            "added_layer_count": len(analysis.added_layers),
            "removed_layer_count": len(analysis.removed_layers),
            "shared_bytes": analysis.shared_bytes,
            "download_bytes": analysis.download_bytes,
            "reuse_ratio": analysis.shared_bytes / total_bytes if total_bytes > 0 else 0,
        }

    output = {
        "images": [_format_image(img) for img in images],
        "updates": [_format_analysis(a) for a in analyses],
        "summary": _calculate_summary(analyses) if analyses else {},
    }

    return json.dumps(output, indent=2)


def _components_by_size(layers: list[LayerInfo]) -> list[str]:
    """Return component names sorted by layer size (largest first)."""
    named = [(layer.component, layer.size) for layer in layers if layer.component]
    named.sort(key=lambda x: x[1], reverse=True)
    return [name for name, _ in named]


_history_fallback_warned = False


def _warn_history_fallback():
    """Print a one-time warning about using history fallback."""
    global _history_fallback_warned
    if not _history_fallback_warned:
        _history_fallback_warned = True
        print("Note: Using OCI history fallback (annotations not available).",
              file=sys.stderr)


def _format_bytes(n: int | float) -> str:
    """Format bytes as human-readable (e.g., 1.5 GiB)."""
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"]:
        if abs(n) < 1024:
            if unit == "B":
                return f"{int(n)} {unit}"
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} PiB"


def _calculate_summary(analyses: list[UpdateAnalysis]) -> dict:
    """Calculate aggregate statistics across all updates."""
    if not analyses:
        return {}

    download_bytes = [a.download_bytes for a in analyses]
    reuse_ratios = []
    for a in analyses:
        total_bytes = a.shared_bytes + a.download_bytes
        if total_bytes > 0:
            reuse_ratios.append(a.shared_bytes / total_bytes)

    return {
        "update_count": len(analyses),
        "avg_reuse_ratio": sum(reuse_ratios) / len(reuse_ratios) if reuse_ratios else 0,
        "avg_download_bytes": int(sum(download_bytes) / len(download_bytes)),
        "min_download_bytes": min(download_bytes),
        "max_download_bytes": max(download_bytes),
        "total_download_bytes": sum(download_bytes),
    }


def die(msg: str):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(1)


def run_output(*args: str) -> str:
    """Run a command and return its stdout."""
    return subprocess.check_output(args, text=True)


if __name__ == "__main__":
    main()
