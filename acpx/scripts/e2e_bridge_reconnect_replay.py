#!/usr/bin/env python3
"""Real end-to-end proof that `acpx-acp-bridge` (the actual binary Zed
launches as a "custom agent" -- a dumb stdio<->WebSocket relay) no longer
silently drops `session/update` notifications across a mid-stream network
drop.

Drives the REAL `acpx-acp-bridge` binary as a subprocess exactly like an
ACP client (Zed) would -- plain JSON-RPC lines over stdin/stdout, zero
knowledge of ACPX's `_acpx.resume` extension on this test's side -- while
a real `acpx-server` backed by a scripted stand-in agent streams
`session/update` notifications roughly once a second. A `socat` TCP
proxy sits between the bridge and the server; killing it mid-stream
simulates a dead network / laptop-sleep style drop (no clean WS close
frame), then a fresh `socat` on the same port simulates the network
coming back.

Asserts every chunk the backend ever emitted is eventually observed on
the bridge's stdout, proving `ResumeTracker` (`acpx-bridge/src/
resume.rs`) transparently recovered whatever was missed during the
outage via `_acpx.resume` -- all without this test, or the ACP client it
stands in for, ever knowing that extension exists. The reader thread
classifies frames continuously in the background (not gated behind any
blocking wait) so the exact chunk count observed before the kill is
whatever the backend/OS scheduler actually delivered by then -- the
assertion cares only that the union of before/after covers every chunk,
not the precise split.

Usage: python3 e2e_bridge_reconnect_replay.py
"""
import json
import os
import socket
import subprocess
import sys
import threading
import time

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVER_BIN = os.path.join(REPO_ROOT, "target", "debug", "acpx-server")
BRIDGE_BIN = os.path.join(REPO_ROOT, "target", "debug", "acpx-acp-bridge")
BACKEND_SCRIPT = "/tmp/acpx_e2e/backend.sh"
SERVER_PORT = 18850
PROXY_PORT = 18851
TOTAL_CHUNKS = 8


def start_server():
    env = dict(os.environ)
    env.update(
        {
            "ACPX_BACKEND_CMD": BACKEND_SCRIPT,
            "ACPX_DEFAULT_AGENT_ID": "stand-in-agent",
            "ACPX_HTTP_BIND": f"127.0.0.1:{SERVER_PORT}",
            "RUST_LOG": "info",
        }
    )
    for stale in ("ACPX_AUTH_TOKEN", "ACPX_ACP_BRIDGE_ENABLED", "ACPX_DB_PATH"):
        env.pop(stale, None)
    log = open("/tmp/acpx_e2e/server.log", "wb")
    proc = subprocess.Popen([SERVER_BIN], env=env, stdout=log, stderr=log)
    for _ in range(100):
        try:
            with socket.create_connection(("127.0.0.1", SERVER_PORT), timeout=0.2):
                return proc
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("acpx-server did not open its port in time")


def start_proxy():
    """Non-forking: this test only ever drives one connection at a time,
    and `socat ...,fork` forks a detached child *per accepted connection*
    -- killing the parent listener leaves that child (and the live
    connection it owns) running, which defeats the whole point of
    simulating a dead connection. Plain (non-fork) mode makes the
    listener process itself the connection, so killing it is a real,
    total severance."""
    log = open("/tmp/acpx_e2e/socat.log", "ab")
    return subprocess.Popen(
        [
            "socat",
            f"TCP-LISTEN:{PROXY_PORT},reuseaddr",
            f"TCP:127.0.0.1:{SERVER_PORT}",
        ],
        stdout=log,
        stderr=log,
    )


class BridgeStdout:
    """Continuously classifies the bridge's stdout in the background so
    nothing depends on this test's own scheduling to keep up in real time
    -- exactly the property the reconnect race in an earlier version of
    this script was missing."""

    def __init__(self, proc):
        self.proc = proc
        self.lock = threading.Lock()
        self.chunks = []
        self.responses = {}
        threading.Thread(target=self._run, daemon=True).start()

    def _run(self):
        for raw in self.proc.stdout:
            raw = raw.decode("utf-8", "replace").strip()
            if not raw:
                continue
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                print(f"    [bridge stdout, non-JSON] {raw}", file=sys.stderr)
                continue
            with self.lock:
                if msg.get("method") == "session/update":
                    text = msg["params"]["update"]["content"]["text"]
                    self.chunks.append(text)
                    print(f"    [bridge -> stdout] {text}")
                elif "id" in msg:
                    self.responses[msg["id"]] = msg

    def wait_for(self, predicate, timeout):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                if predicate(self.chunks, self.responses):
                    return
            time.sleep(0.05)
        raise TimeoutError("timed out waiting for expected bridge stdout state")


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode("utf-8"))
    proc.stdin.flush()


def main():
    os.makedirs("/tmp/acpx_e2e", exist_ok=True)
    print("== starting scratch acpx-server ==")
    server = start_server()
    bridge = None
    proxy = None
    try:
        print("== starting socat proxy (bridge <-> server) ==")
        proxy = start_proxy()
        time.sleep(0.3)

        env = dict(os.environ)
        env["ACPX_ACP_BRIDGE_URL"] = f"ws://127.0.0.1:{PROXY_PORT}/ws"
        bridge = subprocess.Popen(
            [BRIDGE_BIN],
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=open("/tmp/acpx_e2e/bridge.log", "wb"),
        )
        out = BridgeStdout(bridge)

        print("== phase 1: initialize + session/new (real ACP client shape, no _acpx anywhere) ==")
        send(bridge, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
        out.wait_for(lambda chunks, responses: 1 in responses, 15)

        send(bridge, {"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}})
        out.wait_for(lambda chunks, responses: 2 in responses, 15)
        with out.lock:
            session_id = out.responses[2]["result"]["sessionId"]
        print(f"session_id={session_id}")

        print("== phase 2: session/prompt -- streams ~one chunk/sec ==")
        send(bridge, {"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "go"}]}})
        # A short fixed window guarantees the kill below lands mid-stream;
        # exactly how many chunks made it through by then is intentionally
        # not pinned down (see the module doc comment).
        time.sleep(0.5)

        with out.lock:
            before_kill = list(out.chunks)
        print(f"== phase 3: hard-kill the proxy after {len(before_kill)} chunk(s) observed so far (simulated network drop) ==")
        proxy.kill()
        proxy.wait()
        kill_time = time.monotonic()
        # Let the backend keep streaming into the *server's* buffer, unseen,
        # while nothing is listening -- this is the exact window that used
        # to be lost for good.
        time.sleep(1.0)
        print(f"    server still alive? {server.poll() is None} (poll={server.poll()})")

        print("== phase 4: bring the network back (fresh socat on the same port) ==")
        proxy = start_proxy()
        time.sleep(0.5)

        print("== phase 5: send a plain session/load, like a real reconnecting ACP client -- no _acpx.resume from this test ==")
        send(bridge, {"jsonrpc": "2.0", "id": 4, "method": "session/load", "params": {"sessionId": session_id, "cwd": "/tmp", "mcpServers": []}})

        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            with out.lock:
                if 3 in out.responses and 4 in out.responses:
                    break
            try:
                with socket.create_connection(("127.0.0.1", SERVER_PORT), timeout=1.0) as s:
                    s.sendall(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                    s.settimeout(2.0)
                    reply = s.recv(200)
                print(f"    [health probe] {reply[:60]!r}")
            except OSError as exc:
                print(f"    [health probe FAILED] {exc}")
            time.sleep(2.0)
        else:
            print("still waiting for id=3/id=4 after 40s of health probing")

        elapsed_since_kill = time.monotonic() - kill_time
        with out.lock:
            seen_chunks = list(out.chunks)
        expected = [f"chunk-{i}" for i in range(1, TOTAL_CHUNKS + 1)]
        print("== SUMMARY ==")
        print(f"before kill: {before_kill}")
        print(f"all observed (before + after resume): {seen_chunks}")
        print(f"wall time from kill to full recovery: {elapsed_since_kill:.1f}s")
        missing = [c for c in expected if c not in seen_chunks]
        if missing:
            print(f"FAIL: missing chunks after reconnect: {missing}")
            sys.exit(1)
        if len(seen_chunks) != len(set(seen_chunks)):
            print(f"NOTE: some chunks were delivered more than once (at-least-once, not a failure): {seen_chunks}")
        print("PASS: every chunk streamed before *and* during the network outage was recovered after reconnect.")
    finally:
        if bridge is not None:
            try:
                bridge.stdin.close()
            except OSError:
                pass
            bridge.kill()
            bridge.wait()
        if proxy is not None:
            proxy.kill()
            proxy.wait()
        server.kill()
        server.wait()


if __name__ == "__main__":
    main()
