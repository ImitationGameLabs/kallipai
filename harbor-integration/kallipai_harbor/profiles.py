"""The operator's LLM profile file: where it lives and whether it is usable.

Standard library only, so these checks stay importable without the Harbor
virtualenv (``adapter`` imports ``harbor``; this module does not).

The operator's LLM configuration: copy ``localprofile.example.toml``
beside this package as ``.localprofile.toml`` and fill it in. The adapter
ships that file into the container as the tagma's own ``profiles.toml``.
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Any

LOCAL_PROFILE = Path(__file__).resolve().parent.parent / ".localprofile.toml"

# The fields the tagma's loader requires on every profile. It has no
# default for ``max_context_window`` (kallipai_adk::profile::model), so a
# delivered file without it fails the tagma's first read; the other three
# name the entries the profile resolves through.
REQUIRED_PROFILE_FIELDS = ("id", "endpoint", "model", "max_context_window")


def resolve_local_profile(path: Path = LOCAL_PROFILE) -> Path:
    """Return the operator's profile file, refusing a broken one loudly.

    Fails during install, before the tagma starts, naming the file and the
    fields it needs, rather than surfacing later as a tagma boot error. The
    checks cover the file's structure; whether the values themselves fit their
    types is left to the tagma's own read.
    """
    if not path.is_file():
        raise FileNotFoundError(
            f"missing {path}: copy localprofile.example.toml beside it "
            f"and fill in its endpoints (family, api_key) and a set "
            f"with at least one profile "
            f"({', '.join(REQUIRED_PROFILE_FIELDS)})"
        )
    try:
        config = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ValueError(f"{path} is not valid TOML: {exc}") from exc
    _check_delivered_shape(path, config)
    return path


def _check_delivered_shape(path: Path, config: dict[str, Any]) -> None:
    """Refuse a shape the tagma cannot use, before the tagma starts.

    Checks the file's structure only — that it declares set tables, that each
    set holds at least one profile, that every profile carries the required
    fields, and that ``default`` names one of them, or that a lone set stands
    without it — and leaves values and the remaining cross-references to the
    tagma's own read. It is a friendlier failure than the tagma's, not a
    mirror of it: some shapes pass here that the tagma then refuses, and a
    file with no set table is refused here that the tagma would read as an
    empty configuration.
    """
    sets = config.get("sets")
    if not isinstance(sets, dict) or not sets:
        raise ValueError(f"{path} declares no [sets.<name>] table")
    default = config.get("default")
    if not default:
        if len(sets) != 1:
            raise ValueError(
                f"{path} declares {len(sets)} sets and no default = \"<name>\""
            )
    elif default not in sets:
        raise ValueError(f"{path} sets default = {default!r}, which names no set")
    for name, entry in sets.items():
        profiles = entry.get("profiles") if isinstance(entry, dict) else None
        if not isinstance(profiles, list) or not profiles:
            raise ValueError(f"{path} set {name!r} holds no [[sets.{name}.profiles]] entry")
        for profile in profiles:
            missing = [
                field
                for field in REQUIRED_PROFILE_FIELDS
                if not isinstance(profile, dict) or field not in profile
            ]
            if missing:
                raise ValueError(
                    f"{path} set {name!r} has a profile without {', '.join(missing)}"
                )
