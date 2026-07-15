"""Black-box stdio bridge coverage against a real ACPX daemon process.

Run after `cargo build --workspace`:

    python3 -m unittest tests.bridge_integration.test_stdio_bridge
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path

from tests.openhands_integration.acp_stdio_client import AcpStdioClient


ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "target" / "debug"
SERVER = TARGET / "acpx-server"
BRIDGE = TARGET / "acpx-acp-bridge"

BACKEND = r"""
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-bridge"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"agentTag":"bridge-backend"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"""


class StdioBridgeTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        if not SERVER.exists() or not BRIDGE.exists():
            self.skipTest("build acpx-server and acpx-acp-bridge before running this test")
        self.temp = tempfile.TemporaryDirectory(prefix="acpx-stdio-bridge-")
        temp = Path(self.temp.name)
        self.backend = temp / "backend.sh"
        self.backend.write_text(BACKEND)
        self.config = temp / "bridge.json"
        self.config.write_text(
            json.dumps(
                {
                    "default_model": "test/model",
                    "models": [
                        {
                            "id": "test/model",
                            "agent_id": "default",
                            "model_id": "backend-model",
                        }
                    ],
                }
            )
        )
        probe = await asyncio.start_server(lambda *_: None, "127.0.0.1", 0)
        port = probe.sockets[0].getsockname()[1]
        probe.close()
        await probe.wait_closed()
        env = os.environ | {
            "ACPX_BACKEND_CMD": f"sh {self.backend}",
            "ACPX_DEFAULT_AGENT_ID": "default",
            "ACPX_HTTP_BIND": f"127.0.0.1:{port}",
            "ACPX_ACP_BRIDGE_ENABLED": "1",
            "ACPX_ACP_BRIDGE_CONFIG_FILE": str(self.config),
            "ACPX_LIFECYCLE_REAPER_ENABLED": "0",
            "ACPX_AUTH_TOKEN": "bridge-test-token",
        }
        self.server = await asyncio.create_subprocess_exec(
            str(SERVER),
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
        self.url = f"ws://127.0.0.1:{port}/acp/ws"
        for _ in range(100):
            try:
                reader, writer = await asyncio.open_connection("127.0.0.1", port)
                writer.close()
                await writer.wait_closed()
                break
            except OSError:
                await asyncio.sleep(0.05)
        else:
            raise AssertionError("acpx-server did not start its bridge listener")

    async def asyncTearDown(self) -> None:
        if self.server.returncode is None:
            self.server.kill()
            await self.server.wait()
        self.temp.cleanup()

    async def test_stdio_bridge_routes_strict_acp_calls_to_shared_daemon(self) -> None:
        async with await AcpStdioClient.spawn(
            BRIDGE,
            "--url",
            self.url,
            env_overrides={
                "ACPX_ACP_BRIDGE_TOKEN": "bridge-test-token",
                "ACPX_ACP_BRIDGE_TENANT": "bridge-test-tenant",
            },
        ) as client:
            await client.initialize()
            session_id = await client.session_new(cwd="/tmp")
            selection = await client.call(
                "session/set_config_option",
                {"sessionId": session_id, "configId": "model", "value": "test/model"},
            )
            self.assertEqual(selection["configOptions"][0]["id"], "model")
            prompt = await client.call(
                "session/prompt",
                {"sessionId": session_id, "prompt": []},
            )
            self.assertEqual(prompt["agentTag"], "bridge-backend")


if __name__ == "__main__":
    unittest.main()
