"""Explicit per-connection activation for the opt-in snapshot anchor format."""
from __future__ import annotations

NAME = 'pin.enable_frontier_anchors'
SETTING_SQL = f"SELECT setting FROM pg_settings WHERE name = '{NAME}'"
PSQL_SETTING_SQL = f"SELECT COALESCE((SELECT setting FROM pg_settings WHERE name = '{NAME}'), 'absent');"


def options(base: dict[str, str], enabled: bool) -> dict[str, str]:
    return {**base, NAME: 'on' if enabled else 'off'}


def require_setting(actual: str | None, enabled: bool) -> str | None:
    # custom guc placeholders do not prove that the loaded binary implements anchors.
    if actual in (None, 'absent') and not enabled:
        return None
    if actual != ('on' if enabled else 'off'):
        raise ValueError(f'{NAME}: registered setting {actual!r} does not match requested {enabled}')
    return actual
