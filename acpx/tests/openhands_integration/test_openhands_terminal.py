"""Terminal (`/api/bash/*`) HTTP coverage against a real, already-running
OpenHands agent-server.

The bash router (`openhands.agent_server.bash_router`) is host-level, not
conversation-scoped -- it is the same shared terminal surface the
agent-canvas frontend's "Terminal" tab drives, independent of which agent
(OpenHands-native or ACP-backed) any given conversation uses. These tests
exercise it directly over HTTP (no `openhands-sdk` conversation object
needed) and, in the last test, prove it keeps working unmodified while a
real acpx-backed conversation is concurrently in flight -- i.e. the
terminal surface and the ACP integration point don't contend with each
other.

Run via (see `README.md` for full prerequisites):

    uv run --with openhands-sdk==1.29.0 --with pytest \\
        pytest tests/openhands_integration/test_openhands_terminal.py -v
"""

from __future__ import annotations

import threading
import time

import pytest

from . import openhands_sdk_driver as driver


def test_execute_bash_command_synchronous(
    agent_server_host: str, session_api_key: str
):
    """`POST /api/bash/execute_bash_command` blocks until the command
    finishes and returns its final stdout/exit_code -- the simplest
    request/response terminal round trip."""
    marker = f"acpx-terminal-{int(time.time())}"
    result = driver.execute_bash_command(
        agent_server_host, session_api_key, f"echo {marker}"
    )
    assert result["exit_code"] == 0, result
    assert marker in (result.get("stdout") or ""), result


def test_execute_bash_command_nonzero_exit_code_surfaces(
    agent_server_host: str, session_api_key: str
):
    """A failing command is a normal `BashOutput` response (HTTP 200) with
    a non-zero `exit_code`, not an HTTP error -- exercises the
    request/response contract's failure path."""
    result = driver.execute_bash_command(
        agent_server_host, session_api_key, "exit 7"
    )
    assert result["exit_code"] == 7, result


def test_start_bash_command_then_poll_for_output(
    agent_server_host: str, session_api_key: str
):
    """`POST /api/bash/start_bash_command` returns immediately with a
    `BashCommand` record; the caller polls `search_bash_events` for the
    matching `BashOutput` -- the async/poll variant of the same terminal
    surface, used by the frontend's live-streaming terminal view."""
    marker = f"acpx-terminal-async-{int(time.time())}"
    command = driver.start_bash_command(
        agent_server_host, session_api_key, f"sleep 0.2 && echo {marker}"
    )
    command_id = command["id"]

    deadline = time.monotonic() + 15
    output = None
    while time.monotonic() < deadline:
        page = driver.search_bash_events(
            agent_server_host, session_api_key, command_id=command_id
        )
        outputs = [
            item
            for item in page["items"]
            if item.get("kind") == "BashOutput" and item.get("exit_code") is not None
        ]
        if outputs:
            output = outputs[-1]
            break
        time.sleep(0.3)

    assert output is not None, "bash command never produced a finished BashOutput"
    assert output["exit_code"] == 0, output
    assert marker in (output.get("stdout") or ""), output


def test_terminal_survives_concurrent_acp_conversation(
    agent_server_host: str,
    session_api_key: str,
    workspace_dir,
):
    """The shared host-level terminal keeps answering normally while a
    real acpx-backed conversation is running concurrently -- proves the
    two surfaces (terminal, ACP subprocess bridge) don't share a lock or
    otherwise block each other inside the agent-server.

    Deliberately does not depend on the `agent_server_pid` fixture (no
    process-tree assertion here, just HTTP-level liveness) -- see
    `README.md`'s "a note on `ps` snapshot stability" for why that
    fixture specifically is best-effort in some sandboxed tool-execution
    environments; this test has no such dependency and should run
    everywhere the agent-server itself is reachable."""
    from openhands.sdk import Conversation

    agent = driver.build_acp_agent(driver.CLAUDE_BACKEND)
    workspace = driver.build_remote_workspace(
        agent_server_host, session_api_key, workspace_dir
    )
    token = f"acpx-terminal-concurrent-{int(time.time())}"

    conversation = Conversation(agent=agent, workspace=workspace, delete_on_close=False)
    try:
        conversation.send_message(
            f"Reply with exactly this token and nothing else: {token}"
        )

        run_error: list[BaseException] = []

        def _run() -> None:
            try:
                conversation.run(blocking=True, timeout=120.0)
            except BaseException as err:  # noqa: BLE001
                run_error.append(err)

        run_thread = threading.Thread(target=_run, daemon=True)
        run_thread.start()

        # While the ACP turn is in flight, fire several terminal round
        # trips through the *same* agent-server process.
        for i in range(3):
            marker = f"acpx-terminal-mid-run-{i}-{int(time.time())}"
            result = driver.execute_bash_command(
                agent_server_host, session_api_key, f"echo {marker}"
            )
            assert result["exit_code"] == 0, result
            assert marker in (result.get("stdout") or ""), result
            time.sleep(0.5)

        run_thread.join(timeout=150.0)
        if run_thread.is_alive():
            raise AssertionError("conversation.run() did not finish in time")
        if run_error:
            raise run_error[0]
    finally:
        conversation.close()
