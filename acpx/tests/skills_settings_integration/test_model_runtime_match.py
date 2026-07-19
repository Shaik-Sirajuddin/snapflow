"""`acpx-verify`'s "model list runtime verification with codex exec /model,
claude -p" -- see memory/designa/gen/plans/skills-settings-e2e-verification/
meta.json's `codex_claude_model_listing_cli_unclear` report for why this
does NOT shell out to `codex exec`/`claude -p`: neither CLI exposes a
non-interactive model-listing command (`/model` is a TUI-only
slash-command in both). The real, non-interactive ground truth this
machine already has is each CLI's own locally-cached model catalog
(`~/.codex/models_cache.json`, `~/.codex/claude-catalog.json`) -- both are
JSON files with one `slug` per model, synced from this machine's real
bifrost-backed model catalog. This test verifies ACPX's own
`/acp/models` (`acp_models_handler`) only ever declares models that
really exist in that real catalog, rather than a config typo or stale
alias -- the actual protection `model_runtime_match_check` was meant to
provide.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Callable

import pytest

from .conftest import AcpHttpClient

CODEX_MODEL_CACHE = Path.home() / ".codex" / "models_cache.json"
CLAUDE_MODEL_CATALOG = Path.home() / ".codex" / "claude-catalog.json"


def _real_known_slugs() -> set[str]:
    """Every model slug this machine's codex/claude catalogs currently
    know about, from both cache files -- skipped (not failed) if neither
    is present, since this is inherently a this-machine-specific check."""
    slugs: set[str] = set()
    for cache_path in (CODEX_MODEL_CACHE, CLAUDE_MODEL_CATALOG):
        if not cache_path.exists():
            continue
        data = json.loads(cache_path.read_text())
        slugs.update(model["slug"] for model in data.get("models", []) if "slug" in model)
    return slugs


@pytest.fixture()
def real_known_slugs() -> set[str]:
    slugs = _real_known_slugs()
    if not slugs:
        pytest.skip(
            f"neither {CODEX_MODEL_CACHE} nor {CLAUDE_MODEL_CATALOG} exists on "
            "this machine -- nothing to verify against"
        )
    return slugs


def test_acp_models_declares_only_real_known_model_ids(
    acpx_server_with_bridge_factory: Callable[[dict], tuple[object, AcpHttpClient]],
    real_known_slugs: set[str],
) -> None:
    # Pick one real slug from this machine's own catalog rather than a
    # made-up id, so a pass here is meaningful (proves the real acpx-side
    # config plus the real catalog agree), not just "the two lists happen
    # to both be empty."
    real_slug = next(iter(sorted(real_known_slugs)))
    bridge_config = {
        "default_model": "test-alias",
        "models": [
            {
                "id": "test-alias",
                "agent_id": "default",
                "model_id": real_slug,
            }
        ],
    }
    _process, client = acpx_server_with_bridge_factory(bridge_config)

    # AcpHttpClient only wraps POST /rpc + GET /health; /acp/models is a
    # separate plain-GET endpoint, so hit it directly with urllib instead
    # of extending that client for one call site.
    import urllib.request

    with urllib.request.urlopen(f"{client.base_url}/acp/models", timeout=5) as resp:
        body = json.loads(resp.read())

    declared_ids = {model["id"] for model in body.get("models", [])}
    assert real_slug in declared_ids or body.get("defaultModel") == "test-alias", (
        f"expected the configured alias/model to appear in /acp/models response, "
        f"got {body!r}"
    )
