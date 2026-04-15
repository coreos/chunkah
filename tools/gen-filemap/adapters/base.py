"""Abstract base class for package manager adapters."""

from __future__ import annotations

import abc
from dataclasses import dataclass, field
from typing import Iterator


@dataclass
class ComponentInfo:
    """A single component (package/element) and the files it owns."""
    name: str
    """Component name, used as the key in the output filemap."""
    files: list[str]
    """Absolute paths of files owned by this component."""
    interval: str = "monthly"
    """Expected update cadence: 'daily', 'weekly', or 'monthly'."""


class PackageAdapter(abc.ABC):
    """Interface that every package-manager adapter must implement.

    Adding support for a new package manager means writing a subclass of this
    class and registering it in ``adapters/__init__.py``.  The subclass must
    implement :meth:`components`; everything else has a sensible default.

    Example skeleton::

        class MyAdapter(PackageAdapter):
            NAME = "mypkg"
            DESCRIPTION = "MyPkg package manager"

            def __init__(self, rootfs: str, **kwargs):
                self.rootfs = rootfs

            def components(self) -> Iterator[ComponentInfo]:
                for pkg in query_mypkg(self.rootfs):
                    yield ComponentInfo(
                        name=pkg.name,
                        files=pkg.file_list(),
                        interval=self.default_interval(pkg),
                    )

    The adapter is instantiated by ``gen_filemap.py`` with the CLI arguments
    converted to keyword arguments, so add whatever ``__init__`` parameters you
    need and document them in ``add_arguments``.
    """

    #: Short identifier used on the CLI (``--adapter NAME``).
    NAME: str = ""
    #: One-line description shown in ``--help``.
    DESCRIPTION: str = ""

    @abc.abstractmethod
    def components(self) -> Iterator[ComponentInfo]:
        """Yield one :class:`ComponentInfo` per component.

        Files that appear in multiple components should be yielded by the last
        component to claim them (later entries win when the filemap is built).
        """

    @classmethod
    def add_arguments(cls, parser) -> None:  # type: ignore[type-arg]
        """Add adapter-specific arguments to *parser*.

        Override this to expose adapter-specific flags.  Arguments are added
        to the same parser as the global flags so keep names namespaced (e.g.
        ``--bst-project`` rather than ``--project``).
        """
