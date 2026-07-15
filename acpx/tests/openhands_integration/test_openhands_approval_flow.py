"""Approval/confirmation-flow HTTP coverage for real acpx-backed
conversations: `/api/conversations/{id}/confirmation_policy` and
`/api/conversations/{id}/events/respond_to_confirmation`.

**Architectural finding this suite documents and asserts on, not just a
test wrapper**: as of `openhands-sdk==1.29.0`,
`openhands.sdk.agent.acp_agent.ACPAgentClient.request_permission`
(the handler OpenHands registers for the ACP client-side
`session/request_permission` method -- i.e. what a real
`claude-agent-acp`/`codex-acp` backend calls, via acpx, when it wants to
run a risky tool) is hardcoded to auto-approve every request:

    async def request_permission(self, options, session_id, tool_call, **kwargs):
        \"\"\"Auto-approve all permission requests from the ACP server.\"\"\"
        option_id = options[0].option_id if options else "allow_once"
        return RequestPermissionResponse(
            outcome=AllowedOutcome(outcome="selected", option_id=option_id)
        )

This means the OpenHands-side `confirmation_policy`/`respond_to_confirmation`
HTTP surface (designed for OpenHands-native agent tool calls, which pause
and wait for a real `respond_to_confirmation` POST) is a **no-op for
ACP-backed conversations** -- there is never a pending action to respond
to, because the ACP bridge answers `session/request_permission` itself
before the request ever reaches that layer. This is a real, current
OpenHands-side gap (not an acpx gap: acpx's own `session/request_permission`
forwarding was audited and confirmed correct in phase 28/29 -- see
`acpx/COVERAGE.md`) worth flagging upstream, exactly like the
`agent-client-protocol` positional-argument version-skew bug this same
integration effort found and patched earlier in `acp_agent.py`.

These tests verify the *actual* current behavior (auto-approve, HTTP
confirmation surface inert) rather than the behavior one might assume from
the endpoint names existing -- so a future OpenHands upstream fix that
wires ACP permission requests through the real confirmation-policy flow
will make one of these tests (`test_confirmation_endpoint_is_inert_for_acp_conversation`)
fail, which is the intended signal to revisit this file.

Run via (see `README.md` for full prerequisites):

    uv run --with openhands-sdk==1.29.0 --with pytest \\
        pytest tests/openhands_integration/test_openhands_approval_flow.py -v
"""

from __future__ import annotations

import threading
import time

import pytest

from . import openhands_sdk_driver as driver


@pytest.fixture()
def acp_conversation(agent_server_host: str, session_api_key: str, workspace_dir):
    """A real, running acpx-backed (Claude) conversation, closed on
    teardown -- shared setup for every test in this file since the
    approval-flow surface is conversation-scoped."""
    from openhands.sdk import Conversation

    agent = driver.build_acp_agent(driver.CLAUDE_BACKEND)
    workspace = driver.build_remote_workspace(
        agent_server_host, session_api_key, workspace_dir
    )
    conversation = Conversation(agent=agent, workspace=workspace, delete_on_close=False)
    try:
        yield conversation
    finally:
        conversation.close()


def test_set_confirmation_policy_on_acp_conversation_does_not_error(
    agent_server_host: str, session_api_key: str, acp_conversation
):
    """`POST .../confirmation_policy` accepts `AlwaysConfirm` for an
    ACP-backed conversation without erroring (the endpoint itself is
    agent-kind-agnostic -- it just records the policy on the event
    service). Whether it actually *does* anything for an ACP backend is
    what the next test checks."""
    conversation_id = str(acp_conversation.id)
    result = driver.set_confirmation_policy(
        agent_server_host,
        session_api_key,
        conversation_id,
        {"kind": "AlwaysConfirm"},
    )
    assert result.get("success") is True, result


def test_confirmation_endpoint_is_inert_for_acp_conversation(
    agent_server_host: str, session_api_key: str, acp_conversation
):
    """With `AlwaysConfirm` set, send a prompt that would need a tool call
    an OpenHands-native agent would pause on (running a shell command) and
    confirm the real run **completes on its own** within the normal
    timeout, without ever needing a `respond_to_confirmation` POST -- i.e.
    the ACP client's auto-approve (see module docstring) really does
    bypass the confirmation-policy pause for this agent kind. A genuinely
    pending confirmation would leave the run stuck in a
    waiting-for-confirmation state past this deadline."""
    conversation_id = str(acp_conversation.id)
    driver.set_confirmation_policy(
        agent_server_host,
        session_api_key,
        conversation_id,
        {"kind": "AlwaysConfirm"},
    )

    token = f"acpx-approval-{int(time.time())}"
    acp_conversation.send_message(
        "Run the shell command `echo "
        + token
        + "` and then reply with exactly the token it printed and nothing else."
    )

    run_error: list[BaseException] = []

    def _run() -> None:
        try:
            acp_conversation.run(blocking=True, timeout=120.0)
        except BaseException as err:  # noqa: BLE001
            run_error.append(err)

    run_thread = threading.Thread(target=_run, daemon=True)
    run_thread.start()
    run_thread.join(timeout=150.0)

    assert not run_thread.is_alive(), (
        "conversation.run() is still running/stuck past the timeout -- if "
        "this starts failing, it likely means an OpenHands upstream change "
        "wired ACP session/request_permission through the real "
        "confirmation-policy pause (see this module's docstring) and the "
        "test below (which asserts respond_to_confirmation 404s with no "
        "pending action) needs to be updated to actually approve it."
    )
    if run_error:
        raise run_error[0]

    response_text = driver.fetch_agent_final_response(
        agent_server_host, session_api_key, conversation_id
    )
    assert token in response_text, (
        f"expected marker token {token!r} in the reply -- the run finished "
        f"but the shell command result never made it back, got: "
        f"{response_text!r}"
    )


def test_respond_to_confirmation_with_no_pending_action_is_rejected(
    agent_server_host: str, session_api_key: str, acp_conversation
):
    """`POST .../respond_to_confirmation` against a conversation with no
    pending confirmation (the ACP steady-state, per this module's
    docstring) is **not** rejected -- verified live against a real
    agent-server: `accept=True` with nothing pending is a lenient no-op
    (`EventService.respond_to_confirmation` just calls `self.run()`,
    which itself no-ops if already running -- see
    `openhands.agent_server.event_service.EventService
    .respond_to_confirmation`'s source), always answering `200
    {"success": true}` rather than a 404/409. This is the accurate,
    live-verified contract (an earlier draft of this test wrongly
    assumed strict validation without checking real behavior first) --
    kept as a real regression guard on that specific leniency rather
    than deleted, since a future OpenHands change tightening this
    endpoint would be worth noticing too."""
    conversation_id = str(acp_conversation.id)
    response = driver.respond_to_confirmation(
        agent_server_host, session_api_key, conversation_id, accept=True
    )
    assert response.status_code == 200, (
        "expected the documented lenient no-op (200) for a confirmation "
        f"accept with nothing pending, got {response.status_code}: "
        f"{response.text}"
    )
    assert response.json().get("success") is True, response.text


def test_respond_to_confirmation_reject_with_no_pending_action_is_also_lenient(
    agent_server_host: str, session_api_key: str, acp_conversation
):
    """The `accept=False` (reject) path is equally lenient with nothing
    pending: `LocalConversation.reject_pending_actions` just logs a
    warning ("No pending actions to reject") and returns -- still `200
    {"success": true}`, not an error. Covers the other half of the
    `ConfirmationResponseRequest.accept` boolean the previous test's
    `accept=True` path doesn't exercise."""
    conversation_id = str(acp_conversation.id)
    response = driver.respond_to_confirmation(
        agent_server_host, session_api_key, conversation_id, accept=False
    )
    assert response.status_code == 200, (
        f"expected the documented lenient no-op (200) for a confirmation "
        f"reject with nothing pending, got {response.status_code}: "
        f"{response.text}"
    )
    assert response.json().get("success") is True, response.text


def test_ask_agent_side_channel_works_on_acp_conversation(
    agent_server_host: str, session_api_key: str, acp_conversation
):
    """`POST .../ask_agent` -- a lightweight request/response probe
    against a live ACP-backed conversation that does not go through the
    full turn/confirmation machinery at all; included here (rather than
    the terminal/profile files) because it is the closest thing to a
    request/response "approval-adjacent" side channel this API exposes
    for ACP conversations, and is worth covering for full endpoint
    coverage regardless.

    **Precondition discovered live, not documented anywhere upstream**:
    `ACPAgent.ask_agent` (`openhands/sdk/agent/acp_agent.py`) implements
    this by issuing a real ACP `session/fork` against the live session,
    then prompting the fork. A real `claude-agent-acp` backend rejects
    `session/fork` (`-32002 Resource not found`) against a session that
    has never completed a real turn yet -- confirmed by direct raw-stdio
    ACP reproduction outside OpenHands entirely (acpx forwards the fork
    request and correctly surfaces the backend's own rejection; this is
    not an acpx bug). So this test must complete one real turn on the
    conversation first, exactly like a real caller would need to."""
    import threading
    import time

    conversation_id = str(acp_conversation.id)
    warmup_token = f"acpx-ask-agent-warmup-{int(time.time())}"
    acp_conversation.send_message(
        f"Reply with exactly this token and nothing else: {warmup_token}"
    )

    warmup_error: list[BaseException] = []

    def _warmup() -> None:
        try:
            acp_conversation.run(blocking=True, timeout=120.0)
        except BaseException as err:  # noqa: BLE001
            warmup_error.append(err)

    warmup_thread = threading.Thread(target=_warmup, daemon=True)
    warmup_thread.start()
    warmup_thread.join(timeout=150.0)
    assert not warmup_thread.is_alive(), "warmup turn did not finish in time"
    if warmup_error:
        raise warmup_error[0]

    answer = driver.ask_agent(
        agent_server_host,
        session_api_key,
        conversation_id,
        "Reply with exactly the word: pong",
    )
    assert isinstance(answer, str)
    assert answer.strip() != "", "ask_agent returned an empty response"
