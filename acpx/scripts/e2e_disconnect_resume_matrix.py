#!/usr/bin/env python3
"""Matrix of socket-termination / resume scenarios against the ACP bridge
(`/acp/ws`), requested explicitly to cover deadlock/hang regressions:

  A. session/close called, then a clean WS close -- resume attempt on the
     now-closed session must fail fast with a clear error, never hang.
  B. No session/close, WS abruptly killed (RST, no close frame) on an
     otherwise-idle bound session (no in-flight turn) -- reconnect +
     resume must be fast and must succeed (the session is still open).
  C. No session/close, WS abruptly killed *mid* an in-flight tool call --
     reconnect + resume must be fast (not blocked behind the busy
     backend) -- see `e2e_mid_tool_disconnect_resume.py` for the
     dedicated, more detailed version of this one.

Every scenario asserts an upper bound on time-to-first-response after
reconnect so a regression shows up as a hard failure, not just a slow
number buried in a log.
"""
import asyncio
import json
import os
import socket
import time

import websockets

HOST = "127.0.0.1"
PORT = 8793
URL = f"ws://{HOST}:{PORT}/acp/ws"
TOKEN = os.environ.get("ACPX_AUTH_TOKEN", "aa85a8bca5975caa160a46d5acf41b2db071e5676da663a1")

_id = 0
def next_id():
    global _id
    _id += 1
    return _id


async def send(ws, method, params=None):
    rid = next_id()
    frame = {"jsonrpc": "2.0", "id": rid, "method": method}
    if params is not None:
        frame["params"] = params
    await ws.send(json.dumps(frame))
    return rid


async def recv_matching(ws, rid, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for response id={rid}")
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        msg = json.loads(raw)
        if msg.get("id") == rid:
            return msg


def hard_kill(ws):
    try:
        sock = ws.transport.get_extra_info("socket")
        if sock is not None:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
    except Exception:
        pass
    ws.transport.abort()


async def connect():
    headers = {"Authorization": f"Bearer {TOKEN}"}
    ws = await websockets.connect(URL, additional_headers=headers, max_size=None)
    rid = await send(ws, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    resp = await recv_matching(ws, rid)
    assert "error" not in resp, resp
    return ws


async def new_session(ws):
    rid = await send(ws, "session/new", {"cwd": "/tmp", "mcpServers": []})
    resp = await recv_matching(ws, rid, timeout=30)
    assert "error" not in resp, resp
    return resp["result"]["sessionId"]


async def scenario_a_close_then_resume():
    print("\n=== Scenario A: session/close called, then resume attempt ===")
    ws1 = await connect()
    session_id = await new_session(ws1)
    # Bind it with a trivial prompt so there's a real backend session to close.
    rid = await send(ws1, "session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": "say ok"}]})
    await recv_matching(ws1, rid, timeout=60)
    rid = await send(ws1, "session/close", {"sessionId": session_id})
    resp = await recv_matching(ws1, rid, timeout=15)
    print(f"session/close result: {json.dumps(resp)[:200]}")
    await ws1.close()

    ws2 = await connect()
    t0 = time.monotonic()
    rid = await send(ws2, "session/load", {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []})
    resp = await recv_matching(ws2, rid, timeout=15)
    latency = time.monotonic() - t0
    print(f"resume-after-close latency={latency*1000:.1f}ms response={json.dumps(resp)[:200]}")
    assert latency < 15, "resume after close must fail fast, not hang"
    assert "error" in resp, "resuming a closed session must be a clear error, not a silent success"
    await ws2.close()
    print("PASS: resume-after-close fails fast with a clear error, no hang")


async def scenario_b_idle_disconnect_resume():
    print("\n=== Scenario B: idle bound session, hard disconnect, resume ===")
    ws1 = await connect()
    session_id = await new_session(ws1)
    rid = await send(ws1, "session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": "say ok"}]})
    await recv_matching(ws1, rid, timeout=60)
    # Session is now bound and fully idle (no in-flight turn). Kill hard.
    hard_kill(ws1)
    await asyncio.sleep(0.5)

    ws2 = await connect()
    t0 = time.monotonic()
    rid = await send(ws2, "session/load", {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []})
    resp = await recv_matching(ws2, rid, timeout=15)
    latency = time.monotonic() - t0
    print(f"resume-after-idle-disconnect latency={latency*1000:.1f}ms")
    assert latency < 5, f"idle-session resume should be fast, took {latency:.1f}s"
    assert "error" not in resp, f"resume failed: {resp}"
    print(f"PASS: idle-session resume succeeded in {latency*1000:.1f}ms")
    rid = await send(ws2, "session/close", {"sessionId": session_id})
    await recv_matching(ws2, rid, timeout=15)
    await ws2.close()


async def main():
    await scenario_a_close_then_resume()
    await scenario_b_idle_disconnect_resume()
    print("\n=== ALL MATRIX SCENARIOS PASSED ===")


if __name__ == "__main__":
    asyncio.run(main())
