"""pytest fixtures for the acpx <-> OpenHands SDK integration suite.

Every fixture here is best-effort/skip-on-failure rather than
hard-failing: this suite attaches to an operator-launched OpenHands dev
stack (agent-server + agent-canvas), it does not start one itself (see
`README.md`'s "assumes a running OpenHands stack" section) -- a host with
no such stack running should see a clean `SKIPPED`, not a confusing
collection error.
"""

from __future__ import annotations

import shutil
import tempfile
from pathlib import Path

import httpx
import pytest

from . import openhands_sdk_driver as driver


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--openhands-host",
        default=driver.DEFAULT_AGENT_SERVER_HOST,
        help="Base URL of the running OpenHands agent-server.",
    )
    parser.addoption(
        "--openhands-session-api-key",
        default=None,
        help="Session API key for the agent-server. Falls back to "
        "OPENHANDS_SESSION_API_KEY or auto-discovery if omitted.",
    )


@pytest.fixture(scope="session")
def agent_server_host(pytestconfig: pytest.Config) -> str:
    return pytestconfig.getoption("--openhands-host")


@pytest.fixture(scope="session")
def session_api_key(pytestconfig: pytest.Config) -> str:
    explicit = pytestconfig.getoption("--openhands-session-api-key")
    try:
        return driver.discover_session_api_key(explicit)
    except driver.SessionApiKeyNotFound as err:
        pytest.skip(str(err))


@pytest.fixture(scope="session")
def agent_server_reachable(agent_server_host: str, session_api_key: str) -> None:
    """Skip the whole session if the agent-server isn't actually
    reachable/authenticated with the resolved key, instead of every test
    failing individually with the same connection/auth error."""
    try:
        response = httpx.get(
            f"{agent_server_host}/api/settings",
            headers={"X-Session-API-Key": session_api_key},
            timeout=5,
        )
    except httpx.HTTPError as err:
        pytest.skip(f"OpenHands agent-server not reachable at {agent_server_host}: {err}")
    if response.status_code == 401:
        pytest.skip(
            f"OpenHands agent-server rejected session_api_key at {agent_server_host} "
            f"(401 Unauthorized) -- pass the current one via "
            f"--openhands-session-api-key/OPENHANDS_SESSION_API_KEY"
        )
    response.raise_for_status()


@pytest.fixture(scope="session")
def agent_server_pid(agent_server_reachable: None) -> int:
    try:
        return driver.discover_agent_server_pid()
    except RuntimeError as err:
        pytest.skip(str(err))


@pytest.fixture()
def workspace_dir():
    """A fresh, empty temp directory per test, used as the conversation's
    `RemoteWorkspace.working_dir` -- removed afterward regardless of
    outcome."""
    path = Path(tempfile.mkdtemp(prefix="acpx-openhands-e2e-"))
    try:
        yield path
    finally:
        shutil.rmtree(path, ignore_errors=True)
