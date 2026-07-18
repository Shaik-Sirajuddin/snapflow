"""WebSocket-transport daemon-restart recovery, with real process-tree
leak/reuse assertions.

`recovery_transport_helpers` (`acpx-session-recovery` plan): closes the
gap `test_recovery_protocol.py` alone leaves open -- that suite only
ever proxies `/rpc`, so it can prove startup recovery restores a
session's *mapping*, but not that the persistent `/ws` transport
(`acpx-server/src/transport/ws.rs`) itself accepts a prompt against a
recovered session without the client having done anything WS-specific
first. It also never looks past the JSON-RPC response to check whether
the *real* backend connector process was actually replaced, as opposed
to merely reporting success while something leaked or a stale pid got
silently reused.

Run after building the debug binary:

    python3 -m unittest tests.recovery_integration.test_recovery_ws
"""

from __future__ import annotations

import os
import socket
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from . import proc_tree
from .acp_http_client import AcpHttpClient

try:
    from .acp_ws_client import AcpWsClient
except ImportError:  # pragma: no cover - exercised by the skip below
    AcpWsClient = None  # type: ignore[assignment,misc]


ROOT = Path(__file__).resolve().parents[2]
SERVER = ROOT / "target" / "debug" / "acpx-server"

# Distinct from `test_recovery_protocol.py`'s `RECORDING_BACKEND` only in
# that its own script file is named identifiably (`recovery-ws-backend.sh`)
# so `proc_tree.descendants_matching` can find it precisely rather than by
# a generic `sh -c` substring that could match any stand-in backend.
RECORDING_BACKEND = r"""
while IFS= read -r line; do
  id=$(echo "$line" | sed -n 's/.*"id":\([^,]*\),.*/\1/p')
  method=$(echo "$line" | grep -o '"method":"[^"]*"' | head -1 | cut -d'"' -f4)
  printf '%s\t%s\n' "$method" "$line" >> "$ACPX_RECOVERY_LOG"
  if [ "$method" = "session/new" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-recovery-ws"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"prompted":true}}\n' "$id"
  fi
done
"""


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


class RecoveryWebSocketTest(unittest.TestCase):
    def setUp(self) -> None:
        if not SERVER.exists():
            self.skipTest("build acpx-server before running recovery integration tests")
        if AcpWsClient is None:
            self.skipTest("pip install -r tests/recovery_integration/requirements.txt")
        self.temp = tempfile.TemporaryDirectory(prefix="acpx-recovery-ws-")
        root = Path(self.temp.name)
        self.db_path = root / "sessions.sqlite3"
        self.log_path = root / "backend.log"
        self.backend_path = root / "recovery-ws-backend.sh"
        self.backend_path.write_text(RECORDING_BACKEND)
        self.backend_path.chmod(0o700)
        self.processes: list[subprocess.Popen[bytes]] = []
        self.ws_clients: list[AcpWsClient] = []

    def tearDown(self) -> None:
        for client in self.ws_clients:
            try:
                client.close()
            except Exception:
                pass
        for process in reversed(self.processes):
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if process.stderr is not None:
                process.stderr.close()
        self.temp.cleanup()

    def start_daemon(self) -> tuple[subprocess.Popen[bytes], AcpHttpClient]:
        port = free_port()
        env = os.environ | {
            "ACPX_BACKEND_CMD": f"sh {self.backend_path}",
            "ACPX_DEFAULT_AGENT_ID": "default",
            "ACPX_HTTP_BIND": f"127.0.0.1:{port}",
            "ACPX_DB_PATH": str(self.db_path),
            "ACPX_STARTUP_SESSION_RECOVERY_ENABLED": "1",
            "ACPX_LIFECYCLE_REAPER_ENABLED": "0",
            "ACPX_RECOVERY_LOG": str(self.log_path),
        }
        process = subprocess.Popen(
            [str(SERVER)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            env=env,
        )
        self.processes.append(process)
        client = AcpHttpClient(f"http://127.0.0.1:{port}")
        for _ in range(100):
            if process.poll() is not None:
                stderr = process.stderr.read().decode("utf-8", errors="replace")
                self.fail(f"acpx-server exited during startup: {stderr}")
            try:
                client.health()
                return process, client
            except OSError:
                time.sleep(0.05)
            except RuntimeError:
                time.sleep(0.05)
        self.fail("acpx-server did not become healthy")

    def ws_client(self, http_client: AcpHttpClient) -> AcpWsClient:
        client = AcpWsClient(http_client.base_url)
        client.connect()
        self.ws_clients.append(client)
        return client

    def test_restart_recovers_a_session_prompted_directly_over_ws_with_no_leaked_process(
        self,
    ) -> None:
        daemon_a, http_a = self.start_daemon()
        ws_a = self.ws_client(http_a)

        created = ws_a.call("session/new", {"cwd": "/workspace", "mcpServers": []})
        session_id = created["sessionId"]
        prompted = ws_a.call(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "before restart"}]},
        )
        self.assertTrue(prompted["prompted"])
        # Both calls' replies, in order, recorded by the client itself --
        # proves this is genuinely reading its own responses off the
        # wire in order, not just trusting `call()`'s return value.
        self.assertEqual(
            [frame.value["result"] for frame in ws_a.frames if frame.kind == "response"],
            [created, prompted],
        )

        # The real backend connector process, found by walking the real
        # OS process tree under the real `acpx-server` pid -- not
        # inferred from the JSON-RPC response alone.
        backend_procs = proc_tree.descendants_matching(
            daemon_a.pid, str(self.backend_path.name)
        )
        self.assertEqual(
            len(backend_procs),
            1,
            f"expected exactly one backend connector process under acpx-server "
            f"pid={daemon_a.pid}, found {backend_procs}",
        )
        backend_pid_a = backend_procs[0].pid

        # Session persistence is intentionally off the routing hot path.
        for _ in range(100):
            if self.db_path.exists():
                break
            time.sleep(0.05)
        else:
            self.fail("session/new was not persisted before restart")

        ws_a.close()
        daemon_a.kill()
        daemon_a.wait(timeout=5)

        # The backend connector's own stdin was piped from the now-dead
        # `acpx-server`; once that pipe closes, its `while read` loop
        # hits EOF and the process exits on its own. A real leak (the
        # bug this asset exists to catch) would show up here as this
        # pid still being alive well past a generous timeout.
        still_alive = proc_tree.wait_until_gone([backend_pid_a], timeout=5.0)
        self.assertEqual(
            still_alive,
            [],
            f"backend connector pid={backend_pid_a} outlived its parent acpx-server "
            f"process -- leaked, not just restarted",
        )

        daemon_b, http_b = self.start_daemon()
        health = http_b.health()
        self.assertEqual(health["status"], "ready")
        self.assertGreaterEqual(health["recovery"]["restored"], 1)

        # No explicit `session/load` from this client -- proving the
        # persistent `/ws` transport itself, not just `/rpc`, honors a
        # session startup recovery already restored before this
        # connection ever existed.
        ws_b = self.ws_client(http_b)
        prompted_after_restart = ws_b.call(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "after restart"}]},
        )
        self.assertTrue(prompted_after_restart["prompted"])

        backend_procs_b = proc_tree.descendants_matching(
            daemon_b.pid, str(self.backend_path.name)
        )
        self.assertEqual(len(backend_procs_b), 1, backend_procs_b)
        self.assertNotEqual(
            backend_procs_b[0].pid,
            backend_pid_a,
            "the post-restart connector must be a genuinely fresh process, "
            "never the pre-restart pid",
        )

        methods = self.log_path.read_text().splitlines()
        load_index = next(
            index for index, line in enumerate(methods) if line.startswith("session/load\t")
        )
        prompt_indices = [
            index for index, line in enumerate(methods) if line.startswith("session/prompt\t")
        ]
        self.assertTrue(prompt_indices)
        self.assertLess(load_index, prompt_indices[-1])


if __name__ == "__main__":
    unittest.main()
