#!/usr/bin/env python3
"""Compare sequential images and report layer sharing statistics.

Example usage for FCOS (after running chunk-image-series.py):

    ./tools/analyze-layer-reuse.py localhost/fedora-coreos-test --compare-originals

This analyzes layer reuse between sequential chunked images and compares
against the original (un-chunked) images to measure chunking effectiveness.
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
    tag: str
    original_tag: str | None  # From annotation org.chunkah.original-tag
    layers: list[LayerInfo]
    total_size: int


@dataclass
class UpdateAnalysis:
    """Analysis of layer changes between two images."""
    from_tag: str
    to_tag: str
    from_original: str | None
    to_original: str | None
    shared_layers: list[LayerInfo]
    added_layers: list[LayerInfo]
    removed_layers: list[LayerInfo]
    shared_bytes: int
    download_bytes: int


def main():
    parser = argparse.ArgumentParser(
        description="Compare sequential images and report layer sharing statistics"
    )
    parser.add_argument(
        "prefix",
        help="containers-storage prefix (e.g., localhost/fcos-chunked)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        dest="json_output",
        help="Output as JSON",
    )
    parser.add_argument(
        "--show-components",
        action="store_true",
        help="Show which components changed in each update",
    )
    parser.add_argument(
        "--compare-originals",
        action="store_true",
        help="Also analyze original images ({prefix}-orig) for comparison",
    )
    args = parser.parse_args()

    try:
        # Find all images with the prefix
        image_refs = find_images(args.prefix)

        if not image_refs:
            die(f"No images found with prefix '{args.prefix}'")

        if len(image_refs) == 1:
            print("Warning: Only 1 image found. Need at least 2 for update analysis.",
                  file=sys.stderr)

        # Get info for each image
        images = []
        for ref in image_refs:
            images.append(get_image_info(ref, args.prefix))

        # Analyze sequential updates
        analyses = []
        for i in range(len(images) - 1):
            analyses.append(analyze_update(images[i], images[i + 1]))

        # Optionally analyze original images for comparison
        orig_images = []
        orig_analyses = []
        if args.compare_originals:
            orig_prefix = f"{args.prefix}-orig"
            orig_refs = find_images(orig_prefix)
            if not orig_refs:
                print(f"Warning: No original images found at '{orig_prefix}'",
                      file=sys.stderr)
            else:
                for ref in orig_refs:
                    orig_images.append(get_image_info(ref, orig_prefix))
                for i in range(len(orig_images) - 1):
                    orig_analyses.append(analyze_update(orig_images[i], orig_images[i + 1]))

        # Output results
        if args.json_output:
            print(format_json_output(images, analyses, orig_images, orig_analyses))
        else:
            print(format_human_output(images, analyses, orig_images, orig_analyses,
                                      args.show_components))

    except subprocess.CalledProcessError as e:
        die(f"Command failed: {e.cmd}")
    except Exception as e:
        die(str(e))


def find_images(prefix: str) -> list[str]:
    """Find all images matching prefix:0, prefix:1, etc."""
    try:
        output = run_output(
            "podman", "images", "--format", "{{.Repository}}:{{.Tag}}",
            prefix
        )
    except subprocess.CalledProcessError:
        return []

    refs = [line.strip() for line in output.strip().split("\n") if line.strip()]

    # Filter to only those with numeric tags and sort by index
    numeric_refs = []
    for ref in refs:
        try:
            idx = _parse_tag_index(ref, prefix)
            numeric_refs.append((idx, ref))
        except ValueError:
            continue

    numeric_refs.sort(key=lambda x: x[0])
    return [ref for _, ref in numeric_refs]


def get_image_info(image_ref: str, prefix: str) -> ImageInfo:
    """Get layer information for an image via skopeo."""
    output = run_output("skopeo", "inspect", f"containers-storage:{image_ref}")
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

    # Get original tag from labels if available
    original_tag = None
    labels = data.get("Labels", {}) or {}
    original_tag = labels.get("org.chunkah.original-tag")

    tag = image_ref.split(":")[-1] if ":" in image_ref else ""

    return ImageInfo(
        ref=image_ref,
        tag=tag,
        original_tag=original_tag,
        layers=layers,
        total_size=total_size,
    )


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
        from_tag=from_img.tag,
        to_tag=to_img.tag,
        from_original=from_img.original_tag,
        to_original=to_img.original_tag,
        shared_layers=shared_layers,
        added_layers=added_layers,
        removed_layers=removed_layers,
        shared_bytes=shared_bytes,
        download_bytes=download_bytes,
    )


def format_human_output(images: list[ImageInfo], analyses: list[UpdateAnalysis],
                        orig_images: list[ImageInfo], orig_analyses: list[UpdateAnalysis],
                        show_components: bool) -> str:
    """Format analysis results for human consumption."""
    lines = []

    # Header
    if images:
        first_tag = images[0].tag
        last_tag = images[-1].tag
        lines.append(f"==> Found {len(images)} chunked images: {images[0].ref.rsplit(':', 1)[0]}:{first_tag} through :{last_tag}")
        lines.append("")

    # Image summary
    lines.append("==> Chunked Image Summary:")
    for img in images:
        original = f" ({img.original_tag})" if img.original_tag else ""
        size_str = _format_bytes(img.total_size)
        lines.append(f"    :{img.tag}{original}  {len(img.layers)} layers, {size_str}")
    lines.append("")

    # Update analysis
    if analyses:
        lines.append("==> Chunked Update Analysis:")
        lines.append("")

        for analysis in analyses:
            from_str = f":{analysis.from_tag}"
            to_str = f":{analysis.to_tag}"
            if analysis.from_original and analysis.to_original:
                from_str += f" ({analysis.from_original})"
                to_str += f" ({analysis.to_original})"

            total_layers = len(analysis.shared_layers) + len(analysis.added_layers)
            reuse_ratio = len(analysis.shared_layers) / total_layers if total_layers > 0 else 0

            lines.append(f"    {from_str} -> {to_str}")
            lines.append(f"    Shared:   {len(analysis.shared_layers):3} layers ({_format_bytes(analysis.shared_bytes)})")
            lines.append(f"    Added:    {len(analysis.added_layers):3} layers ({_format_bytes(analysis.download_bytes)} download)")
            lines.append(f"    Removed:  {len(analysis.removed_layers):3} layers")
            lines.append(f"    Reuse:    {reuse_ratio * 100:.1f}%")

            if show_components and analysis.added_layers:
                added_components = sorted(set(
                    layer.component for layer in analysis.added_layers
                    if layer.component
                ))
                if added_components:
                    lines.append(f"    Changed components: {', '.join(added_components[:5])}")
                    if len(added_components) > 5:
                        lines.append(f"                        ... and {len(added_components) - 5} more")

            lines.append("")

    # Summary statistics for chunked
    if analyses:
        summary = _calculate_summary(analyses)
        lines.append("==> Chunked Summary:")
        lines.append(f"    Total updates analyzed: {summary['update_count']}")
        lines.append(f"    Average layer reuse:    {summary['avg_reuse_ratio'] * 100:.1f}%")
        lines.append(f"    Average download size:  {_format_bytes(summary['avg_download_bytes'])}")

        if summary['update_count'] > 1:
            lines.append(f"    Min download:           {_format_bytes(summary['min_download_bytes'])}")
            lines.append(f"    Max download:           {_format_bytes(summary['max_download_bytes'])}")
        lines.append("")

    # Original image comparison (if available)
    if orig_images and orig_analyses:
        lines.append("==> Original (un-chunked) Update Analysis:")
        lines.append("")

        for analysis in orig_analyses:
            total_layers = len(analysis.shared_layers) + len(analysis.added_layers)
            reuse_ratio = len(analysis.shared_layers) / total_layers if total_layers > 0 else 0

            lines.append(f"    :{analysis.from_tag} -> :{analysis.to_tag}")
            lines.append(f"    Shared:   {len(analysis.shared_layers):3} layers ({_format_bytes(analysis.shared_bytes)})")
            lines.append(f"    Added:    {len(analysis.added_layers):3} layers ({_format_bytes(analysis.download_bytes)} download)")
            lines.append(f"    Reuse:    {reuse_ratio * 100:.1f}%")
            lines.append("")

        orig_summary = _calculate_summary(orig_analyses)
        lines.append("==> Original Summary:")
        lines.append(f"    Average layer reuse:    {orig_summary['avg_reuse_ratio'] * 100:.1f}%")
        lines.append(f"    Average download size:  {_format_bytes(orig_summary['avg_download_bytes'])}")
        lines.append("")

        # Comparison
        if analyses:
            chunked_summary = _calculate_summary(analyses)
            lines.append("==> Comparison (Chunked vs Original):")
            savings = orig_summary['avg_download_bytes'] - chunked_summary['avg_download_bytes']
            savings_pct = (savings / orig_summary['avg_download_bytes'] * 100
                          if orig_summary['avg_download_bytes'] > 0 else 0)
            lines.append(f"    Download savings:       {_format_bytes(savings)} ({savings_pct:.1f}% smaller)")
            lines.append(f"    Layer reuse improvement: {(chunked_summary['avg_reuse_ratio'] - orig_summary['avg_reuse_ratio']) * 100:+.1f}%")

    return "\n".join(lines)


def format_json_output(images: list[ImageInfo], analyses: list[UpdateAnalysis],
                       orig_images: list[ImageInfo],
                       orig_analyses: list[UpdateAnalysis]) -> str:
    """Format analysis results as JSON."""

    def _format_image(img: ImageInfo) -> dict:
        return {
            "ref": img.ref,
            "tag": img.tag,
            "original_tag": img.original_tag,
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
        total = len(analysis.shared_layers) + len(analysis.added_layers)
        return {
            "from": analysis.from_tag,
            "to": analysis.to_tag,
            "from_original": analysis.from_original,
            "to_original": analysis.to_original,
            "shared_layer_count": len(analysis.shared_layers),
            "added_layer_count": len(analysis.added_layers),
            "removed_layer_count": len(analysis.removed_layers),
            "shared_bytes": analysis.shared_bytes,
            "download_bytes": analysis.download_bytes,
            "reuse_ratio": len(analysis.shared_layers) / total if total > 0 else 0,
        }

    output = {
        "chunked": {
            "images": [_format_image(img) for img in images],
            "updates": [_format_analysis(a) for a in analyses],
            "summary": _calculate_summary(analyses) if analyses else {},
        },
    }

    if orig_images or orig_analyses:
        output["original"] = {
            "images": [_format_image(img) for img in orig_images],
            "updates": [_format_analysis(a) for a in orig_analyses],
            "summary": _calculate_summary(orig_analyses) if orig_analyses else {},
        }

        # Add comparison if both have data
        if analyses and orig_analyses:
            chunked_summary = _calculate_summary(analyses)
            orig_summary = _calculate_summary(orig_analyses)
            savings = orig_summary['avg_download_bytes'] - chunked_summary['avg_download_bytes']
            output["comparison"] = {
                "download_savings_bytes": savings,
                "download_savings_ratio": (
                    savings / orig_summary['avg_download_bytes']
                    if orig_summary['avg_download_bytes'] > 0 else 0
                ),
                "reuse_improvement": (
                    chunked_summary['avg_reuse_ratio'] - orig_summary['avg_reuse_ratio']
                ),
            }

    return json.dumps(output, indent=2)


def _parse_tag_index(ref: str, prefix: str) -> int:
    """Extract numeric index from image reference."""
    if ":" not in ref:
        raise ValueError("No tag in reference")
    tag = ref.split(":")[-1]
    return int(tag)


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
        total = len(a.shared_layers) + len(a.added_layers)
        if total > 0:
            reuse_ratios.append(len(a.shared_layers) / total)

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
