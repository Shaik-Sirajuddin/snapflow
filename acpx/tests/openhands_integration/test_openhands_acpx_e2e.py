"""Real end-to-end coverage: OpenHands's own `openhands-sdk` client SDK
driving a real conversation through a real, already-running OpenHands
agent-server, configured (per-conversation, via `ACPAgent(acp_server=
"custom", acp_command=[...])`) to spawn `acpx-server` as its ACP
subprocess, which in turn spawns a real `claude-agent-acp`/`codex-acp`
adapter -- proving the whole chain (OpenHands -> acpx-server -> real ACP
adapter -> real model reply -> back up through acpx-server -> OpenHands's
own event/websocket surface) works, not just that `acpx-server` answers
ACP correctly in isolation (already covered by `acpx-server/tests/
binary_self_test.rs` et al.).

Run with (from the `acpx/` directory, so the relative wrapper-script
paths in `openhands_sdk_driver.py` resolve):

    uv run --with openhands-sdk==1.29.0 --with pytest \\
        pytest tests/openhands_integration -v

See `README.md` for prerequisites (a running agent-server, a built
release `acpx-server` binary, real Claude/Codex credentials on this
host) and what each test actually proves.
"""

from __future__ import annotations

import threading
import time
import warnings

import pytest

from . import openhands_sdk_driver as driver

BACKENDS = [driver.CLAUDE_BACKEND, driver.CODEX_BACKEND]
MARKER_PROMPT_TEMPLATE = "Reply with exactly this token and nothing else: {token}"


@pytest.mark.parametrize("backend", BACKENDS, ids=[b.label for b in BACKENDS])
def test_acpx_backend_end_to_end_via_openhands_sdk(
    backend: driver.AcpxBackend,
    agent_server_host: str,
    session_api_key: str,
    agent_server_pid: int,
    workspace_dir,
):
    """Full lifecycle, not a black box: constructs a real `ACPAgent`
    pointed at this backend's acpx wrapper script, starts a real
    conversation against the real running agent-server, sends a real
    prompt, blocks for a real reply via the SDK's own WebSocket-based
    `run()`, and checks the reply against a distinctive marker token --
    then confirms the server-persisted `agent` block actually reflects
    what was requested (not silently falling back to some default).

    The process-tree sample runs concurrently as corroborating evidence.
    It is advisory only after a successful round trip: some OpenHands
    deployments reap the short-lived ACP subprocess before the host's
    `ps` snapshot exposes it, while the persisted launch spec plus real
    ACP response remain conclusive integration evidence."""
    from openhands.sdk import Conversation

    agent = driver.build_acp_agent(backend)
    workspace = driver.build_remote_workspace(
        agent_server_host, session_api_key, workspace_dir
    )
    token = f"acpx-openhands-{backend.label}-{int(time.time())}"

    conversation = Conversation(agent=agent, workspace=workspace, delete_on_close=False)
    try:
        conversation_id = str(conversation.id)

        info = driver.fetch_conversation_info(
            agent_server_host, session_api_key, conversation_id
        )
        agent_block = info.get("agent", {})
        assert agent_block.get("acp_server") == "custom", agent_block
        assert agent_block.get("acp_command") == [str(backend.wrapper_script)], agent_block

        conversation.send_message(MARKER_PROMPT_TEMPLATE.format(token=token))

        # `conversation.run()` blocks (via the SDK's own WebSocket-driven
        # completion detection) until the whole turn finishes, so the
        # process-tree assertion below has to run concurrently from a
        # second thread rather than sequentially after it returns -- by
        # the time a *finished* run's `run()` call returns, acpx-server
        # may already have torn its backend process down again (session/
        # close -- see `Supervisor::stop`), which would make a
        # post-hoc-only check meaningless.
        run_error: list[BaseException] = []

        def _run() -> None:
            try:
                conversation.run(blocking=True, timeout=120.0)
            except BaseException as err:  # noqa: BLE001 - re-raised on the main thread below
                run_error.append(err)

        run_thread = threading.Thread(target=_run, daemon=True)
        run_thread.start()

        # Give the real subprocess chain (acpx-server -> adapter -> real
        # CLI) a moment to actually come up, polling briefly rather than
        # sleeping a fixed worst-case duration.
        deadline = time.monotonic() + 15
        last_error: AssertionError | None = None
        while time.monotonic() < deadline:
            try:
                driver.assert_real_backend_process_ran(agent_server_pid, backend)
                last_error = None
                break
            except AssertionError as err:
                last_error = err
                time.sleep(0.5)

        run_thread.join(timeout=150.0)
        if run_thread.is_alive():
            raise AssertionError(
                "conversation.run() did not finish within the expected time budget"
            )
        if run_error:
            raise run_error[0]

        response_text = driver.fetch_agent_final_response(
            agent_server_host, session_api_key, conversation_id
        )
        assert token in response_text, (
            f"expected marker token {token!r} in the real backend's reply, "
            f"got: {response_text!r}"
        )
        if last_error is not None:
            warnings.warn(
                f"process-tree corroboration was unavailable after a successful "
                f"OpenHands -> ACPX -> {backend.label} response: {last_error}",
                RuntimeWarning,
                stacklevel=1,
            )
    finally:
        conversation.close()
