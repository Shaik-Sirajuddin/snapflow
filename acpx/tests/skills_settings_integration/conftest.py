"""pytest fixtures for the designa v2 skills/settings verification suite.

Reuses `recovery_integration`'s stdlib-only HTTP client and its
build-then-spawn-a-real-`acpx-server` pattern (see that package's
`test_recovery_protocol.py`), following `openhands_integration`'s
skip-cleanly-if-unavailable fixture style instead of `recovery_integration`'s
`unittest.TestCase.skipTest` (pytest fixtures are the newer, preferred
convention in this `tests/` directory -- `openhands_integration` already
established it, this suite follows that one rather than adding a third
style).

See `memory/designa/gen/plans/skills-settings-e2e-verification/
01-architecture.md` for the harness's role in the larger plan.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import tempfile
from pathlib import Path
from typing import Callable, Iterator

import pytest

from ..recovery_integration.acp_http_client import AcpHttpClient

ROOT = Path(__file__).resolve().parents[2]

# A synthetic (no real LLM, no network/credentials) backend, same shape as
# recovery_integration's RECORDING_BACKEND: replies to session/new and
# echoes every other method back as a bare success so the harness can
# prove real request/response plumbing without needing a live provider.
SYNTHETIC_BACKEND = r"""
while IFS= read -r line; do
  id=$(echo "$line" | sed -n 's/.*"id":\([^,]*\),.*/\1/p')
  method=$(echo "$line" | grep -o '"method":"[^"]*"' | head -1 | cut -d'"' -f4)
  if [ "$method" = "session/new" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"skills-settings-synthetic"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"""


def _find_server_binary() -> Path | None:
    # Prefer a release build (faster to run repeatedly) but fall back to
    # debug -- recovery_integration only ever checks target/debug, but a
    # release-only checkout (as built for this pass) should not spuriously
    # skip every test in this suite.
    for profile in ("release", "debug"):
        candidate = ROOT / "target" / profile / "acpx-server"
        if candidate.exists():
            return candidate
    return None


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


@pytest.fixture(scope="session")
def acpx_server_binary() -> Path:
    binary = _find_server_binary()
    if binary is None:
        pytest.skip(
            "build acpx-server before running skills_settings_integration "
            "tests: cd acpx && cargo build -p acpx-server (or --release)"
        )
    return binary


def _spawn_acpx_server(
    acpx_server_binary: Path, tmp: Path, extra_env: dict[str, str]
) -> tuple[subprocess.Popen, AcpHttpClient]:
    db_path = tmp / "sessions.sqlite3"
    backend_path = tmp / "backend.sh"
    backend_path.write_text(SYNTHETIC_BACKEND)
    backend_path.chmod(0o700)

    port = free_port()
    env = os.environ | {
        "ACPX_BACKEND_CMD": f"sh {backend_path}",
        "ACPX_DEFAULT_AGENT_ID": "default",
        "ACPX_HTTP_BIND": f"127.0.0.1:{port}",
        "ACPX_DB_PATH": str(db_path),
        "ACPX_LIFECYCLE_REAPER_ENABLED": "0",
    } | extra_env
    process = subprocess.Popen(
        [str(acpx_server_binary)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        env=env,
    )
    client = AcpHttpClient(f"http://127.0.0.1:{port}")
    for _ in range(100):
        if process.poll() is not None:
            stderr = process.stderr.read().decode("utf-8", errors="replace")
            pytest.fail(f"acpx-server exited during startup: {stderr}")
        try:
            client.health()
            break
        except Exception:
            pass
        import time

        time.sleep(0.1)
    else:
        pytest.fail("acpx-server never became healthy")
    return process, client


@pytest.fixture()
def acpx_server(acpx_server_binary: Path) -> Iterator[tuple[subprocess.Popen, AcpHttpClient]]:
    """Launches a real `acpx-server` against the synthetic backend above and
    yields (process, client); always torn down, even on test failure."""
    with tempfile.TemporaryDirectory(prefix="acpx-skills-settings-") as tmp:
        root = Path(tmp)
        process, client = _spawn_acpx_server(acpx_server_binary, root, {})
        try:
            yield process, client
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if process.stderr is not None:
                process.stderr.close()


@pytest.fixture()
def acpx_server_with_bridge_factory(
    acpx_server_binary: Path,
) -> Iterator[Callable[[dict], tuple[subprocess.Popen, AcpHttpClient]]]:
    """Factory fixture (needed because the bridge config's *content* must
    be known before the server process starts, unlike the plain
    `acpx_server` fixture): call it with a `bridge_config: dict` (the
    JSON body `acpx_bridge::BridgeConfig` deserializes -- `default_model`
    + `models: [{id, agent_id, model_id}, ...]`) to get back
    (process, client). Enables `ACPX_ACP_BRIDGE_ENABLED=1` so
    `/acp/models` (`acp_models_handler`) returns the configured models
    instead of 404. Every server spawned through this factory is torn
    down when the test ends, regardless of how many were spawned."""
    spawned: list[subprocess.Popen] = []
    with tempfile.TemporaryDirectory(prefix="acpx-skills-settings-bridge-") as tmp:
        root = Path(tmp)

        def spawn(bridge_config: dict) -> tuple[subprocess.Popen, AcpHttpClient]:
            config_path = root / f"bridge-{len(spawned)}.json"
            config_path.write_text(json.dumps(bridge_config))
            process, client = _spawn_acpx_server(
                acpx_server_binary,
                root,
                {
                    "ACPX_ACP_BRIDGE_ENABLED": "1",
                    "ACPX_ACP_BRIDGE_CONFIG_FILE": str(config_path),
                },
            )
            spawned.append(process)
            return process, client

        try:
            yield spawn
        finally:
            for process in reversed(spawned):
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                if process.stderr is not None:
                    process.stderr.close()
