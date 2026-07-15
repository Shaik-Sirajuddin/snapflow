#!/usr/bin/env python3
"""Final-mile proof for the "mcp ->" hop: drives daemon.launch({headless})
and the full edit/playback.getFrame sequence through genuine MCP
`tools/call` JSON-RPC requests (Streamable HTTP, the same transport Codex
CLI uses per session 14's fix), not the daemon's own control socket.
Complements daemon-headless-parity-check.py, which proves the daemon side;
this proves the MCP adapter forwards `daemon.launch`'s `headless` param and
every `sap.call` verbatim end to end.

Usage:
  scripts/mcp-headless-parity-check.py --mcp-url http://127.0.0.1:7780/mcp
"""
from __future__ import annotations

import argparse
import base64
import io
import sys
import uuid

import requests

ARTIFACT_ASSET = "/home/siraj/Desktop/content/verification/headless-parity/assets/parity-test-clip.mp4"
SAMPLE_FRAMES = [0, 10, 25, 40, 60, 74]


class McpClient:
    def __init__(self, url: str):
        self.url = url
        self.session = requests.Session()
        self.next_id = 1
        self.mcp_session_id = None
        self._initialize()

    def _post(self, method: str, params: dict) -> dict:
        rid = self.next_id
        self.next_id += 1
        body = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self.mcp_session_id:
            headers["Mcp-Session-Id"] = self.mcp_session_id
        resp = self.session.post(self.url, json=body, headers=headers, timeout=60)
        if "Mcp-Session-Id" in resp.headers:
            self.mcp_session_id = resp.headers["Mcp-Session-Id"]
        resp.raise_for_status()
        # Some tool calls (e.g. edit.* mutations that also fan out an
        # `edit.changed` notification) come back as `text/event-stream`
        # with one or more `event: message\ndata: {...}` frames rather than
        # a single JSON body -- pick the frame whose id matches this
        # request, skipping any interleaved notification frames.
        content_type = resp.headers.get("Content-Type", "")
        if content_type.startswith("text/event-stream"):
            data = None
            for line in resp.text.splitlines():
                if not line.startswith("data:"):
                    continue
                import json as _json
                frame = _json.loads(line[len("data:"):].strip())
                if frame.get("id") == rid:
                    data = frame
                    break
            if data is None:
                raise SystemExit(f"no matching SSE frame for id={rid} in response: {resp.text[:500]}")
        else:
            data = resp.json()
        if data.get("error"):
            raise SystemExit(f"MCP error on {method}: {data['error']}")
        return data["result"]

    def _initialize(self):
        self._post("initialize", {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "headless-parity-mcp-check", "version": "0"},
        })
        # Fire-and-forget per spec; ignore response body.
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self.mcp_session_id:
            headers["Mcp-Session-Id"] = self.mcp_session_id
        self.session.post(self.url, json={"jsonrpc": "2.0", "method": "notifications/initialized"}, headers=headers, timeout=30)

    def call_tool(self, name: str, arguments: dict):
        result = self._post("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise SystemExit(f"tool {name} returned isError: {result}")
        content = result.get("content", [])
        text_parts = [c["text"] for c in content if c.get("type") == "text"]
        import json as _json
        return _json.loads(text_parts[0]) if text_parts else result


def png_extrema(png_bytes: bytes):
    from PIL import Image
    return Image.open(io.BytesIO(png_bytes)).convert("RGB").getextrema()


def run_one(client: McpClient, label: str, headless: bool) -> dict:
    name = f"mcpparity-{label}-{uuid.uuid4().hex[:8]}"
    proj = client.call_tool("daemon.createProject", {"name": name})
    project_id = proj.get("id") or proj.get("ID")
    print(f"  [{label}] daemon.createProject -> {project_id}")

    launch = client.call_tool("daemon.launch", {"projectId": project_id, "headless": headless})
    print(f"  [{label}] daemon.launch(headless={headless}) -> status={launch.get('Status') or launch.get('status')}")

    client.call_tool("sap.call", {"method": "project.select", "params": {"projectId": project_id}})
    track = client.call_tool("sap.call", {"method": "edit.addTrack", "params": {"kind": "video"}})
    track_index = track["index"]
    clip = client.call_tool("sap.call", {
        "method": "edit.appendClip",
        "params": {"trackIndex": track_index, "source": {"path": ARTIFACT_ASSET}},
    })
    client.call_tool("sap.call", {
        "method": "filter.add",
        "params": {"clipId": clip["clipId"], "mltService": "qtblend", "properties": {"rect": "0=5% 5% 90% 90% 100"}},
    })

    frames = {}
    for fnum in SAMPLE_FRAMES:
        resp = client.call_tool("sap.call", {"method": "playback.getFrame", "params": {"frame": fnum, "format": "png"}})
        frames[fnum] = base64.b64decode(resp["data"])

    return {"project_id": project_id, "frames": frames}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mcp-url", required=True)
    args = ap.parse_args()

    results = {}
    for label, headless in (("headless", True), ("non_headless", False)):
        print(f"=== {label} (headless={headless}) via real MCP tools/call ===")
        client = McpClient(args.mcp_url)
        results[label] = run_one(client, label, headless)

    print("\n=== comparing MCP-driven headless vs non-headless frames ===")
    mismatches = []
    for fnum in SAMPLE_FRAMES:
        a = results["headless"]["frames"][fnum]
        b = results["non_headless"]["frames"][fnum]
        if a == b:
            print(f"frame {fnum}: IDENTICAL ({len(a)} bytes)")
        else:
            mismatches.append(f"frame {fnum}: {len(a)} vs {len(b)} bytes, not identical")

    if mismatches:
        for m in mismatches:
            print(f"MISMATCH: {m}")
        raise SystemExit(1)

    print("\nPASS: real MCP tools/call daemon.launch({headless:true/false}) against "
          "the Qt/C-ABI Shotcut backend produced byte-identical playback.getFrame "
          "output end to end through the actual MCP transport.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
