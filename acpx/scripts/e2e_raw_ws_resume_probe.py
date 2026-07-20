#!/usr/bin/env python3
"""Minimal isolation probe: does a *raw* WS reconnect-with-cursor (no
acpx-acp-bridge, no socat -- this script IS the client) against a fast
scripted backend hang the same way `e2e_bridge_reconnect_replay.py`
observed? Narrows whether that's a bridge-side issue or a pre-existing
router/WS-transport issue.
"""
import asyncio
import json
import socket
import time

import websockets

PORT = 18860
URL = f"ws://127.0.0.1:{PORT}/ws"

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
            raise TimeoutError(f"timed out waiting for id={rid}")
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        msg = json.loads(raw)
        if msg.get("method") == "session/update":
            text = msg["params"]["update"]["content"]["text"]
            print(f"    [update] {text}")
        elif msg.get("id") == rid:
            return msg


def hard_kill(ws):
    try:
        sock = ws.transport.get_extra_info("socket")
        if sock is not None:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
    except Exception as exc:
        print(f"    (linger setup failed: {exc})")
    ws.transport.abort()


async def main():
    print("== connect, initialize, session/new ==")
    ws1 = await websockets.connect(URL, max_size=None)
    rid = await send(ws1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    resp = await recv_matching(ws1, rid)
    assert "error" not in resp, resp

    rid = await send(ws1, "session/new", {"cwd": "/tmp", "mcpServers": []})
    resp = await recv_matching(ws1, rid, timeout=30)
    assert "error" not in resp, resp
    session_id = resp["result"]["sessionId"]
    print(f"session_id={session_id}")

    print("== session/prompt, capture cursor from live updates, kill after 2 chunks ==")
    rid = await send(ws1, "session/prompt", {"sessionId": session_id, "prompt": [{"type": "text", "text": "go"}]})
    last_seq = None
    last_epoch = None
    seen = []
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline and len(seen) < 2:
        raw = await asyncio.wait_for(ws1.recv(), timeout=deadline - time.monotonic())
        msg = json.loads(raw)
        if msg.get("method") == "session/update":
            params = msg["params"]
            seen.append(params["update"]["content"]["text"])
            ext = params.get("_acpx", {})
            if "seq" in ext:
                last_seq, last_epoch = ext["seq"], ext.get("epoch")
            print(f"    [update before kill] {seen[-1]} seq={last_seq}")

    print(f"== hard-kill after {seen} (last_seq={last_seq}, epoch={last_epoch}) ==")
    hard_kill(ws1)
    await asyncio.sleep(1.5)

    print("== reconnect fresh, session/load with real _acpx.resume cursor ==")
    ws2 = await websockets.connect(URL, max_size=None)
    rid = await send(ws2, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    resp = await recv_matching(ws2, rid)
    assert "error" not in resp, resp

    resume_start = time.monotonic()
    params = {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}
    if last_seq is not None:
        params["_acpx"] = {"resume": {"lastSeq": last_seq, "epoch": last_epoch}}
    rid = await send(ws2, "session/load", params)

    print("== drain until session/prompt (original id) AND session/load resolve ==")
    original_prompt_id = 3  # initialize=1, session/new=2, session/prompt=3
    seen_after = []
    got_prompt_reply = False
    got_load_reply = False
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and not (got_prompt_reply and got_load_reply):
        try:
            raw = await asyncio.wait_for(ws2.recv(), timeout=deadline - time.monotonic())
        except asyncio.TimeoutError:
            break
        msg = json.loads(raw)
        if msg.get("method") == "session/update":
            text = msg["params"]["update"]["content"]["text"]
            seen_after.append(text)
            print(f"    [update after resume] {text} (t+{time.monotonic()-resume_start:.2f}s)")
        elif msg.get("id") == original_prompt_id:
            got_prompt_reply = True
            print(f"    [session/prompt reply] {msg} (t+{time.monotonic()-resume_start:.2f}s)")
        elif msg.get("id") == rid:
            got_load_reply = True
            print(f"    [session/load reply] {msg} (t+{time.monotonic()-resume_start:.2f}s)")

    print("== SUMMARY ==")
    print(f"before kill: {seen}")
    print(f"after resume: {seen_after}")
    print(f"got_prompt_reply={got_prompt_reply} got_load_reply={got_load_reply}")
    all_seen = seen + seen_after
    expected = [f"chunk-{i}" for i in range(1, 9)]
    missing = [c for c in expected if c not in all_seen]
    print(f"missing: {missing}")


if __name__ == "__main__":
    asyncio.run(main())
