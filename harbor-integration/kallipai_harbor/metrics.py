"""Token-usage totals read from a run's agent manifests.

``agents/`` holds one directory per life stage (``active``, ``archived``,
``inactive``) and one directory per agent inside each stage, so a
manifest sits at ``agents/{stage}/{uuid}/manifest.json``. Every stage
counts: an agent archived or parked mid-run still spent what it spent,
and ``cumulative_usage`` is per-agent rather than aggregated.

``cache_read_tokens`` is the read side of a provider's prompt cache. The
adapter passes that total on as the run's cache count, so the cache-write
side stays out of it.

The module imports nothing from Harbor on purpose, so the totals
are testable with the standard library alone.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path


def agent_usage_totals(
    agents_dir: Path, logger: logging.Logger | None = None
) -> tuple[int, int, int]:
    """Sum prompt, completion and cache-read tokens across one agent tree.

    Returns zeroes when the tree does not exist. A manifest that cannot be
    read is skipped: one half-written file must not take the whole total
    down, and the skip is logged for whoever reads the run afterwards.
    """
    prompt = completion = cache = 0
    if not agents_dir.is_dir():
        return (0, 0, 0)
    for manifest in agents_dir.glob("*/*/manifest.json"):
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            if logger is not None:
                logger.debug("failed to parse %s", manifest, exc_info=True)
            continue
        usage = data.get("cumulative_usage") or {}
        prompt += usage.get("prompt_tokens", 0)
        completion += usage.get("completion_tokens", 0)
        cache += usage.get("cache_read_tokens", 0)
    return (prompt, completion, cache)
