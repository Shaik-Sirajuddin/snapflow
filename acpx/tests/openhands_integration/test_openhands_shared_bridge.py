"""OpenHands -> stdio bridge -> shared ACPX daemon integration.

This test attaches to an operator-run OpenHands server and a separately
started bridge-enabled ACPX daemon. It is opt-in because the daemon's model
aliases and credentials are deployment-specific.
"""

from __future__ import annotations

import os
import pytest

from . import openhands_sdk_driver as driver


@pytest.mark.parametrize(
    "model_alias",
    [alias for alias in os.environ.get("ACPX_OPENHANDS_BRIDGE_MODELS", "").split(",") if alias],
)
def test_openhands_conversation_uses_shared_acpx_bridge(
    model_alias: str,
    agent_server_host: str,
    session_api_key: str,
    workspace_dir,
):
    url = os.environ.get("ACPX_OPENHANDS_BRIDGE_URL")
    if not url:
        pytest.skip("set ACPX_OPENHANDS_BRIDGE_URL to a bridge-enabled ACPX /acp/ws URL")

    from openhands.sdk import Conversation

    agent = driver.build_shared_bridge_agent(model_alias)
    workspace = driver.build_remote_workspace(agent_server_host, session_api_key, workspace_dir)
    conversation = Conversation(
        agent=agent,
        workspace=workspace,
        delete_on_close=False,
        secrets=driver.shared_bridge_secrets(
            url,
            token=os.environ.get("ACPX_OPENHANDS_BRIDGE_TOKEN"),
            tenant=os.environ.get("ACPX_OPENHANDS_BRIDGE_TENANT"),
        ),
    )
    try:
        conversation.send_message("Give a concise acknowledgement that the ACP connection is working.")
        conversation.run(blocking=True, timeout=120.0)
        response = driver.fetch_agent_final_response(
            agent_server_host, session_api_key, str(conversation.id)
        )
        assert response.strip(), response
        assert response != "(No response from ACP server)", response
    finally:
        conversation.close()
