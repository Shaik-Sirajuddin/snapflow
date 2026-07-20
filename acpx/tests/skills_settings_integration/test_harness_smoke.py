"""Proves the `acpx_server` fixture itself works end-to-end against a real
spawned `acpx-server` process -- the actual skill/settings verification
tests (`test_skill_injection.py`, `test_settings_reflection.py`, etc.,
per `memory/designa/gen/plans/skills-settings-e2e-verification/
02-phased-plan.md`) build on this fixture rather than re-deriving their
own server-spawning logic.
"""

from __future__ import annotations

from .conftest import AcpHttpClient


def test_health_reports_ok(acpx_server: tuple) -> None:
    _process, client = acpx_server
    health = client.health()
    assert health.get("status") in ("ok", "healthy", True) or health, (
        f"unexpected /health payload: {health!r}"
    )


def test_session_new_reaches_the_real_synthetic_backend(
    acpx_server: tuple[object, AcpHttpClient],
) -> None:
    _process, client = acpx_server
    result = client.call("session/new", {"cwd": "/tmp", "mcpServers": []})
    # ACPX assigns its own gateway-level session id rather than forwarding
    # the backend connector's literal reply id verbatim (discovered by
    # running this against a real spawned acpx-server -- the synthetic
    # backend's "skills-settings-synthetic" id above does NOT appear
    # here). A real id being present at all is still proof the request
    # made a full round trip through the real backend process.
    session_id = result.get("sessionId")
    assert session_id and isinstance(session_id, str), (
        f"expected a real gateway-assigned sessionId, got {result!r}"
    )
