"""Multi-session coverage: two real, concurrently-running acpx-backed
conversations against the same OpenHands agent-server, proving session
isolation end to end -- not just that acpx isolates sessions at its own
layer (already covered by `acpx-core`'s own concurrency/multi-tenancy
test suite, see `acpx/COVERAGE.md` phase 19), but that the whole chain
(OpenHands conversation -> per-conversation acpx-server subprocess ->
real backend adapter -> real model reply -> back through OpenHands's own
per-conversation event/websocket surface) keeps two simultaneous sessions
correctly separated with no cross-talk.

Per `Supervisor`'s process-per-agent lifecycle (see
`acpx-conductor/src/supervisor.rs`), OpenHands spawning two conversations
with `acp_server="custom"` means two independent `acpx-server` processes,
each with its own backend adapter child -- this suite asserts on that
directly via the process tree, in addition to the HTTP-level response
content check.

Run via (see `README.md` for full prerequisites -- this file in
particular burns two real, concurrent model calls per run):

    uv run --with openhands-sdk==1.29.0 --with pytest \\
        pytest tests/openhands_integration/test_openhands_multi_session.py -v
"""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field

import pytest

from . import openhands_sdk_driver as driver
from . import proc_tree


@dataclass
class _SessionResult:
    conversation_id: str | None = None
    token: str | None = None
    response_text: str | None = None
    acpx_pids: list[int] = field(default_factory=list)
    error: BaseException | None = None


def _run_one_session(
    agent_server_host: str,
    session_api_key: str,
    workspace_dir,
    backend: driver.AcpxBackend,
    result: _SessionResult,
) -> None:
    from openhands.sdk import Conversation

    agent = driver.build_acp_agent(backend)
    workspace = driver.build_remote_workspace(
        agent_server_host, session_api_key, workspace_dir
    )
    token = f"acpx-multisession-{backend.label}-{uuid_hex()}"
    result.token = token

    conversation = Conversation(agent=agent, workspace=workspace, delete_on_close=False)
    try:
        result.conversation_id = str(conversation.id)
        conversation.send_message(
            f"Reply with exactly this token and nothing else: {token}"
        )
        conversation.run(blocking=True, timeout=150.0)
        result.response_text = driver.fetch_agent_final_response(
            agent_server_host, session_api_key, result.conversation_id
        )
    except BaseException as err:  # noqa: BLE001
        result.error = err
    finally:
        conversation.close()


def uuid_hex() -> str:
    import uuid

    return uuid.uuid4().hex[:10]


def test_two_concurrent_claude_sessions_stay_isolated(
    agent_server_host: str,
    session_api_key: str,
):
    """Two concurrent conversations, same backend (Claude), each with a
    distinct marker token and a distinct scratch workspace directory,
    started and run truly concurrently from two threads. Asserts:

    1. Each conversation's final response contains *only its own* marker
       token (no cross-talk / response mixing between sessions).
    2. The process tree shows two *distinct* acpx-server pids concurrently
       alive at some point during the run (real process-per-session
       isolation, not serialized-and-mistaken-for-concurrent).

    Deliberately does not depend on the `agent_server_pid` fixture (which
    hard-skips the whole test if `ps` can't see the agent-server process
    -- a real limitation in some sandboxed tool-execution environments,
    see `README.md`'s "a note on `ps` snapshot stability"). Assertion 1
    (the one that actually proves session isolation -- no response
    cross-talk) always runs; assertion 2 (the process-tree corroboration)
    is attempted best-effort inline and skipped with a clear inline note
    if this specific process can't locate the agent-server pid at all,
    rather than skipping the whole test and losing assertion 1's real
    coverage along with it.
    """
    import shutil
    import tempfile
    from pathlib import Path

    backend = driver.CLAUDE_BACKEND
    workspace_a = Path(tempfile.mkdtemp(prefix="acpx-multisession-a-"))
    workspace_b = Path(tempfile.mkdtemp(prefix="acpx-multisession-b-"))
    result_a = _SessionResult()
    result_b = _SessionResult()

    # A background poller records every distinct acpx-server pid seen
    # under the agent-server's process tree while both sessions are
    # in flight, so the "were there really two concurrent processes"
    # assertion doesn't depend on catching an exact instant. Resolved
    # here (not via the `agent_server_pid` fixture) so a resolution
    # failure only disables assertion 2, not the whole test -- see the
    # docstring above.
    agent_server_pid = proc_tree.find_agent_server_pid()
    seen_pids: set[int] = set()
    stop_polling = threading.Event()

    def _poll_pids() -> None:
        if agent_server_pid is None:
            return
        while not stop_polling.is_set():
            for proc in proc_tree.descendants_matching(agent_server_pid, "acpx-server"):
                seen_pids.add(proc.pid)
            time.sleep(0.3)

    poll_thread = threading.Thread(target=_poll_pids, daemon=True)
    poll_thread.start()

    try:
        thread_a = threading.Thread(
            target=_run_one_session,
            args=(agent_server_host, session_api_key, workspace_a, backend, result_a),
        )
        thread_b = threading.Thread(
            target=_run_one_session,
            args=(agent_server_host, session_api_key, workspace_b, backend, result_b),
        )
        thread_a.start()
        thread_b.start()
        thread_a.join(timeout=180.0)
        thread_b.join(timeout=180.0)
    finally:
        stop_polling.set()
        poll_thread.join(timeout=5.0)
        shutil.rmtree(workspace_a, ignore_errors=True)
        shutil.rmtree(workspace_b, ignore_errors=True)

    assert not thread_a.is_alive(), "session A did not finish within the timeout"
    assert not thread_b.is_alive(), "session B did not finish within the timeout"
    if result_a.error:
        raise result_a.error
    if result_b.error:
        raise result_b.error

    assert result_a.conversation_id != result_b.conversation_id
    assert result_a.token in (result_a.response_text or ""), result_a
    assert result_b.token in (result_b.response_text or ""), result_b
    # No cross-talk: session A's reply must not carry session B's token
    # and vice versa.
    assert result_b.token not in (result_a.response_text or ""), (
        result_a,
        result_b,
    )
    assert result_a.token not in (result_b.response_text or ""), (
        result_a,
        result_b,
    )

    if agent_server_pid is None:
        pytest.skip(
            "response-isolation assertions above passed (the real proof "
            "of multi-session isolation); skipping only the process-tree "
            "corroboration because this process could not locate the "
            "agent-server pid via `ps` -- see this test's docstring"
        )
    assert len(seen_pids) >= 2, (
        f"expected at least two distinct acpx-server pids across the two "
        f"concurrent sessions, only observed {seen_pids} -- sessions may "
        f"not actually be running as isolated processes"
    )


def test_conversation_search_lists_both_sessions_independently(
    agent_server_host: str, session_api_key: str, workspace_dir
):
    """`GET /api/conversations/search` -- a lighter-weight multi-session
    check that does not spend a real model call: creates two
    conversations (without running them) and confirms the search listing
    surfaces both with independent ids and independent
    `workspace.working_dir`s."""
    from openhands.sdk import Conversation

    import shutil
    import tempfile
    from pathlib import Path

    backend = driver.CLAUDE_BACKEND
    ws_a = Path(tempfile.mkdtemp(prefix="acpx-multisession-search-a-"))
    ws_b = Path(tempfile.mkdtemp(prefix="acpx-multisession-search-b-"))

    conv_a = Conversation(
        agent=driver.build_acp_agent(backend),
        workspace=driver.build_remote_workspace(agent_server_host, session_api_key, ws_a),
        delete_on_close=False,
    )
    conv_b = Conversation(
        agent=driver.build_acp_agent(backend),
        workspace=driver.build_remote_workspace(agent_server_host, session_api_key, ws_b),
        delete_on_close=False,
    )
    try:
        id_a, id_b = str(conv_a.id), str(conv_b.id)
        assert id_a != id_b

        found_ids: set[str] = set()
        page_id = None
        for _ in range(50):  # bounded pagination walk
            page = driver.search_conversations(
                agent_server_host, session_api_key, limit=50, page_id=page_id
            )
            found_ids.update(item["id"] for item in page["items"])
            page_id = page.get("next_page_id")
            if not page_id or {id_a, id_b} <= found_ids:
                break

        assert id_a in found_ids, f"conversation A ({id_a}) missing from search results"
        assert id_b in found_ids, f"conversation B ({id_b}) missing from search results"

        info_a = driver.fetch_conversation_info(agent_server_host, session_api_key, id_a)
        info_b = driver.fetch_conversation_info(agent_server_host, session_api_key, id_b)
        assert (
            info_a["workspace"]["working_dir"] != info_b["workspace"]["working_dir"]
        ), (info_a, info_b)
    finally:
        conv_a.close()
        conv_b.close()
        shutil.rmtree(ws_a, ignore_errors=True)
        shutil.rmtree(ws_b, ignore_errors=True)
