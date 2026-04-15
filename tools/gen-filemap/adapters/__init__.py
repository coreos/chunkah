"""Adapter registry for gen-filemap.

To add a new build-system adapter:
1. Create ``adapters/<name>.py`` implementing :class:`~base.PackageAdapter`.
2. Import and register it here.
"""

from .base import PackageAdapter, ComponentInfo
from .bst import BstAdapter

ADAPTERS: dict[str, type[PackageAdapter]] = {
    BstAdapter.NAME: BstAdapter,
}

__all__ = ["ADAPTERS", "PackageAdapter", "ComponentInfo"]
