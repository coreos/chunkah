#!/usr/bin/env python3
"""gen-filemap — generate a chunkah filemap from a build-system artifact database.

The output is a JSON file suitable for placement at
``usr/lib/chunkah/filemap.json`` inside the OCI rootfs.  chunkah auto-detects
that path and uses it for rechunking without any extra flags.

Usage
-----
::

    gen-filemap --adapter bst \\
        --bst-project ~/dakota \\
        --bst-target  oci/bluefin.bst \\
        --bst-cmd     "just bst" \\
        --output      filemap.json

    # Or write directly into a checked-out rootfs:
    gen-filemap --adapter bst ... \\
        --output /path/to/rootfs/usr/lib/chunkah/filemap.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from adapters import ADAPTERS


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="gen-filemap",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--adapter",
        required=True,
        choices=sorted(ADAPTERS),
        metavar="ADAPTER",
        help=f"Build-system adapter to use. Available: {', '.join(sorted(ADAPTERS))}",
    )
    parser.add_argument(
        "--output",
        "-o",
        default="-",
        metavar="FILE",
        help="Output path (default: stdout). Use '-' for stdout.",
    )
    parser.add_argument(
        "--indent",
        type=int,
        default=2,
        metavar="N",
        help="JSON indentation level (default: 2; use 0 for compact)",
    )

    # Let each adapter add its own flags
    for adapter_cls in ADAPTERS.values():
        group = parser.add_argument_group(f"{adapter_cls.NAME} adapter options")
        adapter_cls.add_arguments(group)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    adapter_cls = ADAPTERS[args.adapter]
    adapter = adapter_cls(**vars(args))

    filemap: dict = {}
    total_files = 0

    print(f"Generating filemap with adapter '{args.adapter}'...", file=sys.stderr)
    for component in adapter.components():
        filemap[component.name] = {
            "interval": component.interval,
            "files": sorted(component.files),
        }
        total_files += len(component.files)

    print(
        f"Done: {len(filemap)} components, {total_files} files.",
        file=sys.stderr,
    )

    indent = args.indent if args.indent > 0 else None
    output_json = json.dumps(filemap, indent=indent, ensure_ascii=False)

    if args.output == "-":
        print(output_json)
    else:
        out_path = Path(args.output)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(output_json + "\n", encoding="utf-8")
        print(f"Written to {out_path}", file=sys.stderr)

    return 0


if __name__ == "__main__":
    sys.exit(main())
