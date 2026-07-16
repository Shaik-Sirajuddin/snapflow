"""Black-box daemon restart coverage for ACPX durable recovery.

Run after building the debug binary:

    python3 -m unittest tests.recovery_integration.test_recovery_protocol
"""

from __future__ import annotations

import os
import socket
import sqlite3
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from .acp_http_client import AcpHttpClient


ROOT = Path(__file__).resolve().parents[2]
SERVER = ROOT / "target" / "debug" / "acpx-server"

RECORDING_BACKEND = r"""
while IFS= read -r line; do
  id=$(echo "$line" | sed -n 's/.*"id":\([^,]*\),.*/\1/p')
  method=$(echo "$line" | grep -o '"method":"[^"]*"' | head -1 | cut -d'"' -f4)
  printf '%s\t%s\n' "$method" "$line" >> "$ACPX_RECOVERY_LOG"
  if [ "$method" = "session/new" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-recovery"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"prompted":true}}\n' "$id"
  fi
done
"""


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


class RecoveryProtocolTest(unittest.TestCase):
    def setUp(self) -> None:
        if not SERVER.exists():
            self.skipTest("build acpx-server before running recovery integration tests")
        self.temp = tempfile.TemporaryDirectory(prefix="acpx-recovery-protocol-")
        root = Path(self.temp.name)
        self.db_path = root / "sessions.sqlite3"
        self.log_path = root / "backend.log"
        self.backend_path = root / "backend.sh"
        self.backend_path.write_text(RECORDING_BACKEND)
        self.backend_path.chmod(0o700)
        self.processes: list[subprocess.Popen[bytes]] = []

    def tearDown(self) -> None:
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

    def test_restart_recovers_before_direct_prompt(self) -> None:
        first, client_a = self.start_daemon()
        created = client_a.call("session/new", {"cwd": "/workspace", "mcpServers": []})
        session_id = created["sessionId"]

        # Session persistence is intentionally off the routing hot path.
        for _ in range(100):
            if self.db_path.exists():
                with sqlite3.connect(self.db_path) as db:
                    row = db.execute(
                        "SELECT gateway_session_id FROM sessions WHERE gateway_session_id = ?",
                        (session_id,),
                    ).fetchone()
                if row is not None:
                    break
            time.sleep(0.05)
        else:
            self.fail("session/new was not persisted before restart")
        first.kill()
        first.wait(timeout=5)

        _, client_b = self.start_daemon()
        health = client_b.health()
        self.assertEqual(health["status"], "ready")
        self.assertGreaterEqual(
            health["recovery"]["restored"],
            1,
            f"recovery health={health}; backend log={self.log_path.read_text()!r}",
        )
        prompted = client_b.call(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": "after restart"}]},
        )
        self.assertTrue(prompted["prompted"])

        methods = self.log_path.read_text().splitlines()
        load_index = next(
            index for index, line in enumerate(methods) if line.startswith("session/load\t")
        )
        prompt_index = next(
            index for index, line in enumerate(methods) if line.startswith("session/prompt\t")
        )
        self.assertLess(load_index, prompt_index)


if __name__ == "__main__":
    unittest.main()
