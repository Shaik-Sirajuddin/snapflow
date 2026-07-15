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
    `proc_tree.find_pid_by_cmd_substring`'s doc comment for why this
    suite attaches to one rather than spawning its own."""
    pid = proc_tree.find_pid_by_cmd_substring("agent-server --host")
    if pid is None:
        raise RuntimeError(
            "no running OpenHands agent-server process found "
            "(looked for a command line containing 'agent-server --host')"
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
