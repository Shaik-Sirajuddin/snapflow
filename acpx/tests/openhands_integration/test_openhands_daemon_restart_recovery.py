"""Opt-in OpenHands -> shared ACPX daemon restart-recovery verification.

This test deliberately does not run by default: it sends real model prompts
and executes an operator-supplied command that restarts a daemon. Enable it
only in a controlled environment:

    ACPX_OPENHANDS_RECOVERY_TEST=1 \
    ACPX_OPENHANDS_BRIDGE_URL=ws://127.0.0.1:8790/acp/ws \
    ACPX_OPENHANDS_RECOVERY_MODEL=claude/sonnet \
    ACPX_OPENHANDS_RECOVERY_RESTART_COMMAND='systemctl --user restart acpx' \
    uv run --with openhands-sdk==1.29.0 --with pytest \
      pytest tests/openhands_integration/test_openhands_daemon_restart_recovery.py -v

`ACPX_OPENHANDS_RECOVERY_HEALTH_URL` optionally overrides the `/health` URL
derived from the bridge URL. `ACPX_OPENHANDS_BRIDGE_TOKEN` is reused for the
health request when daemon transport authentication is enabled.

The current stdio bridge exits when its upstream WebSocket closes. This test
therefore proves durable daemon recovery and a new OpenHands bridge connection
after restart; it intentionally does not claim that a pre-restart OpenHands
ACP subprocess reconnects in place.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import time
import uuid
from urllib.parse import urlsplit, urlunsplit

import httpx
import pytest

from . import openhands_sdk_driver as driver


_ENABLED = os.environ.get("ACPX_OPENHANDS_RECOVERY_TEST") == "1"

# A collection-time gate is intentional: with the default environment pytest
# must not contact OpenHands, a model backend, or a daemon control command.
pytestmark = pytest.mark.skipif(
    not _ENABLED,
    reason=(
        "set ACPX_OPENHANDS_RECOVERY_TEST=1 to run the real daemon restart "
        "recovery test"
    ),
)


def _required_env(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        pytest.skip(f"set {name} to run the daemon restart recovery test")
    return value


def _health_url(bridge_url: str) -> str:
    explicit = os.environ.get("ACPX_OPENHANDS_RECOVERY_HEALTH_URL", "").strip()
    if explicit:
        return explicit.rstrip("/")

    parsed = urlsplit(bridge_url)
    if parsed.scheme not in {"ws", "wss"} or not parsed.netloc:
        pytest.skip(
            "ACPX_OPENHANDS_BRIDGE_URL must be a ws:// or wss:// URL, or set "
            "ACPX_OPENHANDS_RECOVERY_HEALTH_URL explicitly"
        )
    scheme = "https" if parsed.scheme == "wss" else "http"
    return urlunsplit((scheme, parsed.netloc, "/health", "", ""))


def _restart_daemon(command: str) -> None:
    args = shlex.split(command)
    if not args:
        pytest.skip("ACPX_OPENHANDS_RECOVERY_RESTART_COMMAND was empty")
    subprocess.run(
        args,
        check=True,
        timeout=60,
        text=True,
        capture_output=True,
    )


def _wait_for_recovered_health(health_url: str, token: str | None) -> dict:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    deadline = time.monotonic() + 90.0
    last_detail = "daemon did not answer"

    while time.monotonic() < deadline:
        try:
            response = httpx.get(health_url, headers=headers, timeout=5.0)
            if response.status_code == 200:
                body = response.json()
                recovery = body.get("recovery", {})
                if body.get("status") == "ready" and recovery.get("restored", 0) >= 1:
                    return body
                last_detail = f"health response {body!r}"
            else:
                last_detail = f"health returned HTTP {response.status_code}: {response.text!r}"
        except (httpx.HTTPError, ValueError) as error:
            last_detail = f"health unavailable: {error}"
        time.sleep(1.0)

    raise AssertionError(
        "ACPX did not become ready with at least one restored durable session "
        f"after restart; {last_detail}"
    )


def _run_bridge_turn(
    *,
    model_alias: str,
    prompt: str,
    agent_server_host: str,
    session_api_key: str,
    workspace_dir,
    bridge_url: str,
):
    from openhands.sdk import Conversation

    conversation = Conversation(
        agent=driver.build_shared_bridge_agent(model_alias),
        workspace=driver.build_remote_workspace(
            agent_server_host, session_api_key, workspace_dir
        ),
        delete_on_close=False,
        secrets=driver.shared_bridge_secrets(
            bridge_url,
            token=os.environ.get("ACPX_OPENHANDS_BRIDGE_TOKEN"),
            tenant=os.environ.get("ACPX_OPENHANDS_BRIDGE_TENANT"),
        ),
    )
    try:
        conversation.send_message(prompt)
        conversation.run(blocking=True, timeout=150.0)
        return driver.fetch_agent_final_response(
            agent_server_host, session_api_key, str(conversation.id)
        )
    finally:
        conversation.close()


def test_openhands_shared_bridge_recovers_after_acpx_daemon_restart(
    agent_server_reachable: None,
    agent_server_host: str,
    session_api_key: str,
    workspace_dir,
):
    """Bind a real OpenHands bridge session, restart the shared daemon, then
    prove startup recovery restored it before a second OpenHands bridge
    connection completes a new turn.

    The first marker makes a durable, recoverable bridge session. `/health`
    verifies startup recovery restored at least that session before the
    post-restart OpenHands call is allowed to run.
    """
    del agent_server_reachable
    bridge_url = _required_env("ACPX_OPENHANDS_BRIDGE_URL")
    model_alias = _required_env("ACPX_OPENHANDS_RECOVERY_MODEL")
    restart_command = _required_env("ACPX_OPENHANDS_RECOVERY_RESTART_COMMAND")
    health_url = _health_url(bridge_url)

    before_token = f"acpx-openhands-recovery-before-{uuid.uuid4().hex[:12]}"
    before_response = _run_bridge_turn(
        model_alias=model_alias,
        prompt=f"Reply with exactly this token and nothing else: {before_token}",
        agent_server_host=agent_server_host,
        session_api_key=session_api_key,
        workspace_dir=workspace_dir / "before-restart",
        bridge_url=bridge_url,
    )
    assert before_token in before_response, before_response

    _restart_daemon(restart_command)
    health = _wait_for_recovered_health(
        health_url, os.environ.get("ACPX_OPENHANDS_BRIDGE_TOKEN")
    )
    assert health["recovery"]["failed"] == 0, health

    after_token = f"acpx-openhands-recovery-after-{uuid.uuid4().hex[:12]}"
    after_response = _run_bridge_turn(
        model_alias=model_alias,
        prompt=f"Reply with exactly this token and nothing else: {after_token}",
        agent_server_host=agent_server_host,
        session_api_key=session_api_key,
        workspace_dir=workspace_dir / "after-restart",
        bridge_url=bridge_url,
    )
    assert after_token in after_response, after_response
