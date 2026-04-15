"""BuildStream adapter for gen-filemap.

Generates a chunkah file-map by querying the local BST artifact cache via
``bst artifact list-contents``.  Each BST element becomes one component.

Requirements
------------
- ``bst`` must be on PATH (or invocable via ``--bst-cmd``).
- The target element (and its dependencies) must be cached locally.
- Run from inside the BST project directory, or pass ``--bst-project``.

Usage
-----
::

    gen-filemap --adapter bst \\
        --bst-project ~/dakota \\
        --bst-target  oci/bluefin.bst \\
        --output      filemap.json
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from typing import Iterator

from .base import ComponentInfo, PackageAdapter


# Intervals are heuristic — BST elements don't carry update cadence metadata.
# Adjust these patterns to match your project's naming conventions.
_INTERVAL_HINTS: list[tuple[str, str]] = [
    # faster-moving Bluefin/GNOME additions
    ("bluefin/", "weekly"),
    ("gnome/gnome-shell", "weekly"),
    ("gnome/mutter", "weekly"),
    ("gnome/gdm", "monthly"),
    # large, slow-moving base layers
    ("freedesktop-sdk.bst", "monthly"),
    ("gnome-build-meta.bst", "monthly"),
]
_DEFAULT_INTERVAL = "monthly"


def _guess_interval(element_name: str) -> str:
    for prefix, interval in _INTERVAL_HINTS:
        if prefix in element_name:
            return interval
    return _DEFAULT_INTERVAL


class BstAdapter(PackageAdapter):
    NAME = "bst"
    DESCRIPTION = "BuildStream — queries local artifact cache via `bst artifact list-contents`"

    def __init__(self, bst_project: str, bst_target: str, bst_cmd: str = "bst", **_kwargs):
        self.project = bst_project
        self.target = bst_target
        self.bst_cmd = bst_cmd

    # ------------------------------------------------------------------
    # PackageAdapter interface
    # ------------------------------------------------------------------

    def components(self) -> Iterator[ComponentInfo]:
        elements = self._list_elements()
        total = len(elements)
        for i, elem in enumerate(elements, 1):
            print(f"  [{i}/{total}] {elem}", end="\r", file=sys.stderr)
            files = self._list_contents(elem)
            if files:
                yield ComponentInfo(
                    name=self._component_name(elem),
                    files=files,
                    interval=_guess_interval(elem),
                )
        print(file=sys.stderr)  # newline after progress

    @classmethod
    def add_arguments(cls, parser: argparse.ArgumentParser) -> None:
        parser.add_argument(
            "--bst-project",
            default=".",
            metavar="DIR",
            help="Path to the BST project directory (default: current directory)",
        )
        parser.add_argument(
            "--bst-target",
            required=True,
            metavar="ELEMENT",
            help="Top-level BST element whose full dependency tree to map "
                 "(e.g. oci/bluefin.bst)",
        )
        parser.add_argument(
            "--bst-cmd",
            default="bst",
            metavar="CMD",
            help="BST executable or wrapper (default: bst). "
                 "Use e.g. 'just bst' if BST runs inside a container.",
        )

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _bst(self, *args: str) -> str:
        """Run a bst subcommand and return stdout."""
        cmd = self.bst_cmd.split() + list(args)
        result = subprocess.run(
            cmd,
            cwd=self.project,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"`{' '.join(cmd)}` failed:\n{result.stderr.strip()}"
            )
        return result.stdout

    def _list_elements(self) -> list[str]:
        """Return all elements in the dependency tree of the target."""
        out = self._bst(
            "show",
            "--format", "%{name}\n",
            "--deps", "all",
            self.target,
        )
        return [line.strip() for line in out.splitlines() if line.strip()]

    def _list_contents(self, element: str) -> list[str]:
        """Return the absolute file paths installed by *element*.

        Returns an empty list if the artifact is not in the local cache.
        """
        try:
            out = self._bst("artifact", "list-contents", "--long", element)
        except RuntimeError:
            return []

        files = []
        for line in out.splitlines():
            # `bst artifact list-contents --long` format:
            #   <element>:
            #       usr/bin/foo   (no leading slash, no type indicator for files)
            line = line.strip()
            if not line or line.endswith(":"):
                continue
            # Skip directory entries (they end with /)
            if line.endswith("/"):
                continue
            # Strip any leading whitespace / tab characters and ensure abs path
            path = "/" + line.lstrip()
            files.append(path)
        return files

    @staticmethod
    def _component_name(element: str) -> str:
        """Convert 'bluefin/gnome-shell.bst' → 'bluefin-gnome-shell'."""
        return element.replace("/", "-").removesuffix(".bst")
