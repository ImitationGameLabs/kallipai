"""Harbor benchmarking adapter for kallipai."""

import importlib

__all__ = ["KallipaiAdapter"]


def __getattr__(name: str):
    # Lazy: importing the adapter pulls in harbor, which is only
    # installed inside benchmarking venvs. Harbor-free consumers
    # (unit tests, path helpers) import kallipai_harbor.tagma directly.
    if name == "KallipaiAdapter":
        return importlib.import_module("kallipai_harbor.adapter").KallipaiAdapter
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
