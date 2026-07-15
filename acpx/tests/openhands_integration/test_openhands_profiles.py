"""Agent-profile (`/api/agent-profiles/*`) HTTP coverage: proves the
profile-store path to acpx (not just the direct `ACPAgent(...)`
construction `test_openhands_acpx_e2e.py` uses) round-trips correctly and
that a conversation created *from a stored profile* actually launches the
real acpx wrapper script it names.

`/api/profiles` (LLM profiles, no ACP relevance) is covered too, briefly,
for HTTP-surface completeness since the objective asks for "profiles"
coverage generally -- the ACP-relevant surface is `/api/agent-profiles`
(`AgentProfile`, `agent_kind: "acp"`), covered in depth below.

Run via (see `README.md` for full prerequisites):

    uv run --with openhands-sdk==1.29.0 --with pytest \\
        pytest tests/openhands_integration/test_openhands_profiles.py -v
"""

from __future__ import annotations

import time
import uuid

import pytest

from . import openhands_sdk_driver as driver


def test_list_llm_profiles_smoke(agent_server_host: str, session_api_key: str):
    """`GET /api/profiles` answers with the `ProfileListResponse` shape --
    no ACP relevance, included for HTTP-surface completeness."""
    result = driver.list_llm_profiles(agent_server_host, session_api_key)
    assert "profiles" in result
    assert "active_profile" in result


def test_list_agent_profiles_includes_seeded_default(
    agent_server_host: str, session_api_key: str
):
    """`GET /api/agent-profiles` -- on a host that has ever activated an
    ACP agent (this workspace's dev stack has, per the phase-28/29 manual
    verification), the lazily-seeded `default` profile is `agent_kind:
    "acp"` and carries a real `acp_server`/`acp_command`."""
    result = driver.list_agent_profiles(agent_server_host, session_api_key)
    assert result["profiles"], "expected at least the lazily-seeded default profile"
    names = {p["name"] for p in result["profiles"]}
    assert "default" in names, result


@pytest.fixture()
def scratch_agent_profile_name() -> str:
    """A per-test unique profile name -- avoids clobbering the real
    `default` profile (or colliding across parallel test runs) and is
    cleaned up afterward regardless of outcome."""
    return f"acpx-test-{uuid.uuid4().hex[:12]}"


@pytest.mark.parametrize(
    "backend", [driver.CLAUDE_BACKEND, driver.CODEX_BACKEND], ids=lambda b: b.label
)
def test_save_and_activate_acp_agent_profile(
    backend: driver.AcpxBackend,
    agent_server_host: str,
    session_api_key: str,
    scratch_agent_profile_name: str,
):
    """Full CRUD + activate round trip for an `ACPAgentProfile` pointed at
    this package's acpx wrapper script:

    1. `POST /api/agent-profiles/{name}` saves it.
    2. `GET /api/agent-profiles/{name}` reads back the exact
       `acp_server`/`acp_command`/`acp_model` fields saved.
    3. `POST /api/agent-profiles/{id}/activate` sets the pointer (does not
       itself mutate `agent_settings` -- see the module the profile body
       came from, `driver.acp_agent_profile_body`'s docstring).
    4. `GET /api/agent-profiles` reflects the new `active_agent_profile_id`.
    """
    name = scratch_agent_profile_name
    body = driver.acp_agent_profile_body(backend)

    try:
        saved = driver.save_agent_profile(
            agent_server_host, session_api_key, name, body
        )
        assert saved["name"] == name, saved

        detail = driver.get_agent_profile(agent_server_host, session_api_key, name)
        profile = detail["profile"]
        assert profile["agent_kind"] == "acp", profile
        assert profile["acp_server"] == "custom", profile
        assert profile["acp_command"] == str(backend.wrapper_script), profile
        assert profile["acp_model"] == backend.acp_model, profile
        profile_id = profile["id"]

        activation = driver.activate_agent_profile(
            agent_server_host, session_api_key, profile_id
        )
        assert activation["id"] == profile_id, activation

        listing = driver.list_agent_profiles(agent_server_host, session_api_key)
        assert listing["active_agent_profile_id"] == profile_id, listing
    finally:
        driver.delete_agent_profile(agent_server_host, session_api_key, name)


def test_materialize_acp_agent_profile_reports_valid(
    agent_server_host: str, session_api_key: str, scratch_agent_profile_name: str
):
    """`POST /api/agent-profiles/{name}/materialize` dry-run resolves the
    profile without launching anything -- ACP profiles have no LLM/MCP refs
    to dangle, so a real one should always report `valid=True`."""
    name = scratch_agent_profile_name
    body = driver.acp_agent_profile_body(driver.CLAUDE_BACKEND)
    try:
        driver.save_agent_profile(agent_server_host, session_api_key, name, body)
        diagnostics = driver.materialize_agent_profile(
            agent_server_host, session_api_key, name
        )
        assert diagnostics.get("valid") is True, diagnostics
    finally:
        driver.delete_agent_profile(agent_server_host, session_api_key, name)


def test_conversation_from_stored_acp_profile_launches_real_backend(
    agent_server_host: str,
    session_api_key: str,
    agent_server_pid: int,
    workspace_dir,
    scratch_agent_profile_name: str,
):
    """The full profile-store path: save an `ACPAgentProfile` naming the
    Claude wrapper script, start a conversation via
    `ACPAgent(acp_server="custom", ...)` matching that stored profile
    (`openhands-sdk`'s `Conversation` API takes the agent spec directly --
    there is no `agent_profile_id=` conversation-creation kwarg exposed by
    this SDK version, so this proves the *stored profile* and the
    *actually-launched agent* agree field-for-field, which is what matters
    for "profiles are correctly wired to acpx"), and confirms the real
    acpx-server + backend adapter process tree comes up exactly like the
    direct-construction path in `test_openhands_acpx_e2e.py` does."""
    from openhands.sdk import Conversation

    backend = driver.CLAUDE_BACKEND
    name = scratch_agent_profile_name
    body = driver.acp_agent_profile_body(backend)

    try:
        driver.save_agent_profile(agent_server_host, session_api_key, name, body)
        stored = driver.get_agent_profile(agent_server_host, session_api_key, name)[
            "profile"
        ]

        agent = driver.build_acp_agent(backend)
        assert agent.acp_command == [stored["acp_command"]] or agent.acp_command == [
            str(backend.wrapper_script)
        ], (agent.acp_command, stored)

        workspace = driver.build_remote_workspace(
            agent_server_host, session_api_key, workspace_dir
        )
        conversation = Conversation(
            agent=agent, workspace=workspace, delete_on_close=False
        )
        try:
            token = f"acpx-profile-{int(time.time())}"
            conversation.send_message(
                f"Reply with exactly this token and nothing else: {token}"
            )

            import threading

            run_error: list[BaseException] = []

            def _run() -> None:
                try:
                    conversation.run(blocking=True, timeout=120.0)
                except BaseException as err:  # noqa: BLE001
                    run_error.append(err)

            run_thread = threading.Thread(target=_run, daemon=True)
            run_thread.start()

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
                raise AssertionError("conversation.run() did not finish in time")
            if run_error:
                raise run_error[0]
            if last_error is not None:
                raise last_error
        finally:
            conversation.close()
    finally:
        driver.delete_agent_profile(agent_server_host, session_api_key, name)
