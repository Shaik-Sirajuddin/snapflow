"""Thin helpers around the *real* `openhands-sdk` client library
(`openhands.sdk.Conversation`/`RemoteWorkspace`/`openhands.sdk.agent.ACPAgent`)
for driving an already-running OpenHands agent-server through a real
acpx-backed ACP conversation end to end.

Deliberately reuses OpenHands's own client SDK rather than hand-rolling a
second HTTP/WS client against its REST API (see this package's
`README.md` for the reasoning): `Conversation(..., workspace=
RemoteWorkspace(...))` returns a `RemoteConversation` that already talks
the real `POST /api/conversations`, `POST .../events`, `POST .../run`,
and `GET/WS /sockets/events/{id}` surface internally (see
`openhands.sdk.conversation.impl.remote_conversation` in the installed
package for the exact wire calls) -- this file only adds the
acpx-specific `ACPAgent` construction and a couple of test-friendly
conveniences (session-api-key discovery, final-response fetch, process-
tree assertions) on top.

Run via `uv run --with openhands-sdk==<pinned version> ...` (see
`README.md`) so this always exercises the exact SDK version the running
agent-server was launched with, rather than whatever happens to be on
`sys.path`.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from . import proc_tree

# Matches this repo's actual layout: `acpx/tests/openhands_integration/` is
# two directories below `acpx/`, which is where the wrapper scripts and
# release binary live.
ACPX_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_DIR = ACPX_ROOT / "scripts"
CLAUDE_WRAPPER = SCRIPTS_DIR / "openhands-acpx-claude.sh"
CODEX_WRAPPER = SCRIPTS_DIR / "openhands-acpx-codex.sh"
SHARED_BRIDGE_WRAPPER = SCRIPTS_DIR / "openhands-acpx-bridge.sh"

DEFAULT_AGENT_SERVER_HOST = "http://127.0.0.1:18000"


class SessionApiKeyNotFound(RuntimeError):
    """Raised when no OpenHands agent-server session API key could be
    discovered by any of `discover_session_api_key`'s strategies."""


def discover_session_api_key(explicit: str | None = None) -> str:
    """Resolve the agent-server's session API key.

    Precedence, matching how an operator would normally reach for one:
    1. `explicit` (a caller-supplied value, e.g. a pytest CLI option).
    2. `OPENHANDS_SESSION_API_KEY` env var.
    3. Parsed off the already-running `agent-canvas` static-file-server
       process's own `--session-api-key` argument (the same value the
       bundled frontend itself uses to reach the agent-server) -- this is
       a best-effort convenience for local/manual runs against an
       operator-launched dev stack, not something to rely on in CI.
    """
    if explicit:
        return explicit
    env_key = os.environ.get("OPENHANDS_SESSION_API_KEY")
    if env_key:
        return env_key
    for proc in proc_tree.snapshot():
        if "static-server.mjs" not in proc.cmd and "agent-canvas" not in proc.cmd:
            continue
        match = re.search(r"--session-api-key\s+(\S+)", proc.cmd)
        if match:
            return match.group(1)
    raise SessionApiKeyNotFound(
        "could not discover an OpenHands session API key -- pass one "
        "explicitly, set OPENHANDS_SESSION_API_KEY, or ensure the "
        "agent-canvas dev stack is running"
    )


def discover_agent_server_pid() -> int:
    """Locate the already-running agent-server process's pid -- see
    `proc_tree.find_agent_server_pid`'s doc comment for why this
    suite attaches to one rather than spawning its own."""
    explicit_pid = os.environ.get("OPENHANDS_AGENT_SERVER_PID")
    if explicit_pid:
        try:
            pid = int(explicit_pid)
        except ValueError as err:
            raise RuntimeError(
                "OPENHANDS_AGENT_SERVER_PID must be a positive integer"
            ) from err
        if pid <= 0:
            raise RuntimeError("OPENHANDS_AGENT_SERVER_PID must be a positive integer")
        return pid

    pid = proc_tree.find_agent_server_pid()
    if pid is None:
        raise RuntimeError(
            "no running OpenHands agent-server process found "
            "(looked for an agent-server executable launched with --host; "
            "set OPENHANDS_AGENT_SERVER_PID if process listing is unreliable)"
        )
    return pid


@dataclass
class AcpxBackend:
    """One acpx-fronted ACP backend this suite knows how to drive --
    either of `acpx/scripts/openhands-acpx-{claude,codex}.sh`. Kept data-
    only (no `openhands_sdk` imports at import time) so
    `test_openhands_acpx_e2e.py` can parametrize over this without
    triggering the (fairly slow, ~5s) SDK import/banner-print twice per
    collection.
    """

    label: str
    wrapper_script: Path
    acp_model: str
    process_marker: str  # substring identifying the real adapter's own process


CLAUDE_BACKEND = AcpxBackend(
    label="claude",
    wrapper_script=CLAUDE_WRAPPER,
    acp_model="sonnet",
    process_marker="claude-agent-acp",
)
CODEX_BACKEND = AcpxBackend(
    label="codex",
    wrapper_script=CODEX_WRAPPER,
    acp_model="gpt-5.5",
    process_marker="codex-acp",
)


def build_acp_agent(backend: AcpxBackend):
    """Construct the real `openhands.sdk.agent.ACPAgent` this suite drives
    OpenHands through -- `acp_server="custom"` + `acp_command=[wrapper]`
    is exactly the integration point `acpx/scripts/openhands-acpx-*.sh`'s
    own doc comments describe; imported lazily (not at module scope) so
    importing this module doesn't require `openhands-sdk` to be
    installed (only actually building/using an agent does)."""
    from openhands.sdk.agent import ACPAgent

    if not backend.wrapper_script.exists():
        raise FileNotFoundError(
            f"{backend.wrapper_script} does not exist -- build the release "
            f"binary first: cd {ACPX_ROOT} && cargo build --release -p acpx-server"
        )
    return ACPAgent(
        acp_command=[str(backend.wrapper_script)],
        acp_server="custom",
        acp_model=backend.acp_model,
    )


def build_shared_bridge_agent(model_alias: str):
    """Build one OpenHands ACP agent backed by the shared ACPX bridge.

    URL/token/tenant are intentionally supplied as `Conversation.secrets`,
    not written into an OpenHands profile or process-wide environment. The
    SDK forwards request secrets only to that ACP subprocess.
    """
    from openhands.sdk.agent import ACPAgent

    if not SHARED_BRIDGE_WRAPPER.exists():
        raise FileNotFoundError(f"missing shared bridge wrapper: {SHARED_BRIDGE_WRAPPER}")
    return ACPAgent(
        acp_command=[str(SHARED_BRIDGE_WRAPPER)],
        acp_server="custom",
        acp_model=model_alias,
    )


def shared_bridge_secrets(
    url: str, *, token: str | None = None, tenant: str | None = None
) -> dict[str, str]:
    """Environment variables consumed by `openhands-acpx-bridge.sh`.

    `token` is optional because a loopback development daemon may omit
    bearer auth; caller-owned values are never persisted in agent profiles.
    """
    secrets = {"ACPX_ACP_BRIDGE_URL": url}
    if token:
        secrets["ACPX_ACP_BRIDGE_TOKEN"] = token
    if tenant:
        secrets["ACPX_ACP_BRIDGE_TENANT"] = tenant
    return secrets


def build_remote_workspace(host: str, api_key: str, working_dir: Path):
    """Construct the real `openhands.sdk.RemoteWorkspace` pointed at the
    already-running agent-server. `working_dir` must be a real path the
    agent-server process (i.e. this same host, for the local dev-stack
    case this suite targets) can resolve."""
    from openhands.sdk import RemoteWorkspace

    working_dir.mkdir(parents=True, exist_ok=True)
    return RemoteWorkspace(
        host=host, api_key=api_key, working_dir=str(working_dir)
    )


def fetch_agent_final_response(host: str, api_key: str, conversation_id: str) -> str:
    """`GET /api/conversations/{id}/agent_final_response` -- see this
    package's README for why this is fetched directly via `httpx` rather
    than through the SDK: `RemoteConversation` has no typed accessor for
    this specific endpoint, and it's a simple enough GET that adding a
    second, parallel client abstraction just to wrap one call would cost
    more clarity than it would save.
    """
    response = httpx.get(
        f"{host.rstrip('/')}/api/conversations/{conversation_id}/agent_final_response",
        headers={"X-Session-API-Key": api_key},
        timeout=30,
    )
    response.raise_for_status()
    return response.json()["response"]


def fetch_conversation_info(host: str, api_key: str, conversation_id: str) -> dict[str, Any]:
    """`GET /api/conversations/{id}` -- used to assert the server-side
    persisted `agent` block actually reflects the `ACPAgent`/`acp_server=
    "custom"`/`acp_command` this suite requested, rather than silently
    falling back to some pre-existing default agent config."""
    response = httpx.get(
        f"{host.rstrip('/')}/api/conversations/{conversation_id}",
        headers={"X-Session-API-Key": api_key},
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def assert_real_backend_process_ran(
    agent_server_pid: int, backend: AcpxBackend
) -> list[proc_tree.ProcInfo]:
    """Assert a real `acpx-server` process, with a real `backend.
    process_marker` process transitively underneath it, is currently
    running somewhere in the agent-server's process tree. Returns the
    matched acpx-server process(es) for the caller to log/inspect further
    if the assertion is about to fail (see the pytest test's own
    diagnostics on failure).

    Best called *while* `conversation.run()` is in flight from a second
    thread/task (the acpx-server process is only alive for the lifetime
    of the conversation, per `Supervisor`'s own process-per-agent
    lifecycle -- see `acpx-conductor/src/supervisor.rs`), not after it
    returns.
    """
    acpx_procs = proc_tree.descendants_matching(agent_server_pid, "acpx-server")
    assert acpx_procs, (
        f"no acpx-server process found under agent-server pid "
        f"{agent_server_pid}'s process tree -- OpenHands never actually "
        f"spawned it via acp_command"
    )
    backend_procs = proc_tree.descendants_matching(
        agent_server_pid, backend.process_marker
    )
    assert backend_procs, (
        f"no {backend.process_marker!r} process found under agent-server "
        f"pid {agent_server_pid}'s process tree -- acpx-server never spawned "
        f"the real ACP adapter"
    )
    return acpx_procs


# ---------------------------------------------------------------------------
# Direct HTTP helpers for agent-server surfaces that `openhands-sdk`'s
# `RemoteConversation` has no typed client for (terminal/bash, LLM + agent
# profiles, confirmation policy/approval flow, conversation search). Kept as
# thin `httpx` wrappers, same rationale as `fetch_agent_final_response`/
# `fetch_conversation_info` above: these are simple enough calls that a
# second parallel client abstraction would cost more clarity than it saves.
# ---------------------------------------------------------------------------


def _headers(api_key: str) -> dict[str, str]:
    return {"X-Session-API-Key": api_key}


# -- Terminal (bash) --------------------------------------------------------


def execute_bash_command(
    host: str, api_key: str, command: str, *, cwd: str | None = None, timeout: int = 60
) -> dict[str, Any]:
    """`POST /api/bash/execute_bash_command` -- runs synchronously (the
    agent-server itself awaits the background task before responding) and
    returns the final `BashOutput` (stdout/stderr/exit_code)."""
    body: dict[str, Any] = {"command": command, "timeout": timeout}
    if cwd is not None:
        body["cwd"] = cwd
    response = httpx.post(
        f"{host.rstrip('/')}/api/bash/execute_bash_command",
        headers=_headers(api_key),
        json=body,
        timeout=timeout + 30,
    )
    response.raise_for_status()
    return response.json()


def start_bash_command(
    host: str, api_key: str, command: str, *, cwd: str | None = None, timeout: int = 60
) -> dict[str, Any]:
    """`POST /api/bash/start_bash_command` -- fires the command in the
    background and returns the `BashCommand` record immediately (caller
    polls `search_bash_events` for its output)."""
    body: dict[str, Any] = {"command": command, "timeout": timeout}
    if cwd is not None:
        body["cwd"] = cwd
    response = httpx.post(
        f"{host.rstrip('/')}/api/bash/start_bash_command",
        headers=_headers(api_key),
        json=body,
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def search_bash_events(
    host: str, api_key: str, *, command_id: str | None = None
) -> dict[str, Any]:
    """`GET /api/bash/bash_events/search` -- optionally filtered to one
    command's `BashCommand`/`BashOutput` events."""
    params: dict[str, str] = {}
    if command_id is not None:
        params["command_id__eq"] = command_id
    response = httpx.get(
        f"{host.rstrip('/')}/api/bash/bash_events/search",
        headers=_headers(api_key),
        params=params,
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


# -- LLM profiles (`/api/profiles`) ------------------------------------------


def list_llm_profiles(host: str, api_key: str) -> dict[str, Any]:
    response = httpx.get(
        f"{host.rstrip('/')}/api/profiles", headers=_headers(api_key), timeout=30
    )
    response.raise_for_status()
    return response.json()


# -- Agent profiles (`/api/agent-profiles`) -- the ACP-relevant surface -----


def list_agent_profiles(host: str, api_key: str) -> dict[str, Any]:
    """`GET /api/agent-profiles` -- launch-spec profiles, including
    `agent_kind: "acp"` entries carrying `acp_server`/`acp_command`. Lazily
    seeds one `default` profile from current settings on first call against
    an empty store (see `agent_profiles_router.list_agent_profiles`)."""
    response = httpx.get(
        f"{host.rstrip('/')}/api/agent-profiles", headers=_headers(api_key), timeout=30
    )
    response.raise_for_status()
    return response.json()


def get_agent_profile(host: str, api_key: str, name: str) -> dict[str, Any]:
    response = httpx.get(
        f"{host.rstrip('/')}/api/agent-profiles/{name}",
        headers=_headers(api_key),
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def save_agent_profile(
    host: str, api_key: str, name: str, profile_body: dict[str, Any]
) -> dict[str, Any]:
    """`POST /api/agent-profiles/{name}` -- creates or overwrites (by
    name) a stored `AgentProfile`. `profile_body` should omit `name` (the
    path segment is authoritative) and match either the
    `OpenHandsAgentProfile` or `ACPAgentProfile` discriminated-union shape
    (`agent_kind: "openhands" | "acp"`)."""
    response = httpx.post(
        f"{host.rstrip('/')}/api/agent-profiles/{name}",
        headers=_headers(api_key),
        json=profile_body,
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def delete_agent_profile(host: str, api_key: str, name: str) -> dict[str, Any]:
    response = httpx.delete(
        f"{host.rstrip('/')}/api/agent-profiles/{name}",
        headers=_headers(api_key),
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def activate_agent_profile(host: str, api_key: str, profile_id: str) -> dict[str, Any]:
    """`POST /api/agent-profiles/{profile_id}/activate` -- pointer-only
    (does not itself write `agent_settings`; a conversation must pass
    `agent_profile_id` at creation time, or the caller must separately
    `materialize` it, to actually apply it)."""
    response = httpx.post(
        f"{host.rstrip('/')}/api/agent-profiles/{profile_id}/activate",
        headers=_headers(api_key),
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def materialize_agent_profile(host: str, api_key: str, name: str) -> dict[str, Any]:
    response = httpx.post(
        f"{host.rstrip('/')}/api/agent-profiles/{name}/materialize",
        headers=_headers(api_key),
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def acp_agent_profile_body(backend: AcpxBackend) -> dict[str, Any]:
    """Build an `ACPAgentProfile`-shaped body pointed at one of this
    package's acpx wrapper scripts -- the profile-store equivalent of
    `build_acp_agent`'s direct `ACPAgent(...)` construction. `acp_command`
    is a single shlex-joined string (the store's on-disk representation;
    see `agent_profiles_router._build_seed_profile`'s own `shlex.join`)."""
    import shlex

    return {
        "agent_kind": "acp",
        "acp_server": "custom",
        "acp_command": shlex.join([str(backend.wrapper_script)]),
        "acp_model": backend.acp_model,
    }


# -- Approval / confirmation-policy flow -------------------------------------


def set_confirmation_policy(
    host: str, api_key: str, conversation_id: str, policy: dict[str, Any]
) -> dict[str, Any]:
    """`POST /api/conversations/{id}/confirmation_policy`. `policy` is a
    `ConfirmationPolicyBase`-shaped dict, e.g. `{"kind": "AlwaysConfirm"}`,
    `{"kind": "NeverConfirm"}`, or `{"kind": "ConfirmRisky", "threshold":
    "HIGH"}`."""
    response = httpx.post(
        f"{host.rstrip('/')}/api/conversations/{conversation_id}/confirmation_policy",
        headers=_headers(api_key),
        json={"policy": policy},
        timeout=30,
    )
    response.raise_for_status()
    return response.json()


def respond_to_confirmation(
    host: str,
    api_key: str,
    conversation_id: str,
    *,
    accept: bool,
    reason: str = "test response",
) -> httpx.Response:
    """`POST /api/conversations/{id}/events/respond_to_confirmation`.
    Returns the raw `httpx.Response` (not `.json()`-decoded/raised) since
    callers of this helper specifically want to assert on status codes,
    including the no-pending-action 4xx case."""
    return httpx.post(
        f"{host.rstrip('/')}/api/conversations/{conversation_id}/events/respond_to_confirmation",
        headers=_headers(api_key),
        json={"accept": accept, "reason": reason},
        timeout=30,
    )


def ask_agent(host: str, api_key: str, conversation_id: str, question: str) -> str:
    """`POST /api/conversations/{id}/ask_agent` -- a side-channel question
    to the agent that does not affect conversation state (used here as a
    lightweight liveness probe against an ACP-backed conversation without
    spending a full billed turn)."""
    response = httpx.post(
        f"{host.rstrip('/')}/api/conversations/{conversation_id}/ask_agent",
        headers=_headers(api_key),
        json={"question": question},
        timeout=60,
    )
    response.raise_for_status()
    return response.json()["response"]


# -- Conversation search (multi-session verification) ------------------------


def search_conversations(
    host: str, api_key: str, *, limit: int = 20, page_id: str | None = None
) -> dict[str, Any]:
    """`GET /api/conversations/search` -- lists conversations known to this
    agent-server (unlike `GET /api/conversations`, which requires an
    explicit `ids` query param and 400s without one)."""
    params: dict[str, Any] = {"limit": limit}
    if page_id is not None:
        params["page_id"] = page_id
    response = httpx.get(
        f"{host.rstrip('/')}/api/conversations/search",
        headers=_headers(api_key),
        params=params,
        timeout=30,
    )
    response.raise_for_status()
    return response.json()
