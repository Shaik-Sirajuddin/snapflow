#!/usr/bin/env python3
"""Real end-to-end test: open a WS session against the ACP bridge
(`/acp/ws` -- the same endpoint Zed connects to), start a prompt that
triggers a long-running tool call, abruptly sever the TCP connection
mid-tool-call (no WS close frame -- simulates a dead network / killed
client, not a clean disconnect), then open a *fresh* WS connection and
resume the same session id using the real `_acpx.resume` cursor
(`lastSeq`/`epoch`) captured from the last live update seen before the
kill. Measures time-to-first-live-frame after reconnect, which should be
near-instant (decoupled from the still-in-flight, now-orphaned backend
turn) rather than blocked behind it.

Usage: python3 e2e_mid_tool_disconnect_resume.py
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
    """Read frames until we see the response with id==rid, printing any
    session/update notifications along the way."""
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for response id={rid}")
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        msg = json.loads(raw)
        if msg.get("method") == "session/update":
            upd = msg.get("params", {}).get("update", {})
            kind = upd.get("sessionUpdate")
            print(f"    [update] {kind}")
        elif msg.get("id") == rid:
            return msg


def hard_kill_connection(ws):
    """Sever the underlying TCP socket abruptly (RST-like), bypassing the
    WS close handshake entirely -- simulates a dead client / network drop,
    not a clean disconnect."""
    try:
        sock = ws.transport.get_extra_info("socket")
        if sock is not None:
            sock.setsockopt(
                socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00"
            )
    except Exception as exc:
        print(f"    (linger setup failed, falling back to abort: {exc})")
    ws.transport.abort()


async def main():
    headers = {"Authorization": f"Bearer {TOKEN}"}

    print("== phase 1: connect, initialize, session/new ==")
    ws1 = await websockets.connect(URL, additional_headers=headers, max_size=None)
    rid = await send(ws1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    resp = await recv_matching(ws1, rid)
    assert "error" not in resp, resp

    rid = await send(ws1, "session/new", {"cwd": "/tmp", "mcpServers": []})
    resp = await recv_matching(ws1, rid, timeout=30)
    assert "error" not in resp, resp
    session_id = resp["result"]["sessionId"]
    print(f"session_id={session_id}")

    print("== phase 2: send prompt that triggers a long-running tool call ==")
    prompt_text = (
        "Run the shell command `sleep 12 && echo tool-call-done` using your "
        "shell/exec tool and report the output. Do not just describe it, actually execute it."
    )
    rid = await send(
        ws1,
        "session/prompt",
        {"sessionId": session_id, "prompt": [{"type": "text", "text": prompt_text}]},
    )

    # Drain updates, capturing the resume cursor (_acpx.seq/epoch) from the
    # last live update seen, until we observe a real tool_call (proof the
    # backend is genuinely mid-turn) or a generous deadline elapses (cold
    # bind + real model reasoning can legitimately take 20-30s).
    saw_tool_call = False
    last_seq = None
    last_epoch = None
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            raw = await asyncio.wait_for(ws1.recv(), timeout=deadline - time.monotonic())
        except asyncio.TimeoutError:
            break
        msg = json.loads(raw)
        if msg.get("method") == "session/update":
            params = msg.get("params", {})
            kind = params.get("update", {}).get("sessionUpdate")
            print(f"    [update before kill] {kind}")
            ext = params.get("_acpx")
            if ext and "seq" in ext:
                last_seq = ext["seq"]
                last_epoch = ext.get("epoch")
            if kind in ("tool_call", "tool_call_update"):
                saw_tool_call = True
                break
    print(f"saw_tool_call_before_kill={saw_tool_call} last_seq={last_seq} last_epoch={last_epoch}")

    print("== phase 3: hard-kill the TCP connection (no close frame) mid-tool-call ==")
    hard_kill_connection(ws1)
    kill_time = time.monotonic()
    await asyncio.sleep(0.5)

    print("== phase 4: reconnect fresh WS, resume the same session (with resume cursor) ==")
    ws2 = await websockets.connect(URL, additional_headers=headers, max_size=None)
    rid = await send(ws2, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    resp = await recv_matching(ws2, rid)
    assert "error" not in resp, resp

    resume_start = time.monotonic()
    resume_params = {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}
    if last_seq is not None:
        resume_params["_acpx"] = {"resume": {"lastSeq": last_seq, "epoch": last_epoch}}
    rid = await send(ws2, "session/load", resume_params)

    print("== phase 5: measure time-to-first-live-frame after reconnect (should be ms, decoupled from the busy backend) ==")
    saw_completion = False
    first_frame_latency = None
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        try:
            raw = await asyncio.wait_for(ws2.recv(), timeout=deadline - time.monotonic())
        except asyncio.TimeoutError:
            break
        msg = json.loads(raw)
        if first_frame_latency is None:
            first_frame_latency = time.monotonic() - resume_start
            print(f"    time to first frame after reconnect: {first_frame_latency*1000:.1f}ms")
        if msg.get("method") == "session/update":
            upd = msg.get("params", {}).get("update", {})
            kind = upd.get("sessionUpdate")
            print(f"    [update after resume] {kind} {json.dumps(upd)[:200]}")
            if kind in ("tool_call_update",) and upd.get("status") == "completed":
                saw_completion = True
        elif "result" in msg or "error" in msg:
            print(f"    [rpc reply after resume] {json.dumps(msg)[:300]}")
            resume_rpc_latency = time.monotonic() - resume_start
            print(f"    session/load RPC reply latency: {resume_rpc_latency*1000:.1f}ms")
            if "result" in msg:
                saw_completion = True

    total_since_kill = time.monotonic() - kill_time
    print("== SUMMARY ==")
    if first_frame_latency is not None:
        print(f"time to first live frame after reconnect: {first_frame_latency*1000:.1f} ms")
    else:
        print("time to first live frame after reconnect: NONE RECEIVED within 40s (hang)")
    print(f"saw tool call completion / turn resolution after resume: {saw_completion}")
    print(f"total wall time from kill to end of observation: {total_since_kill:.1f}s")

    await send(ws2, "session/close", {"sessionId": session_id})
    await asyncio.sleep(0.2)
    await ws2.close()


if __name__ == "__main__":
    asyncio.run(main())
