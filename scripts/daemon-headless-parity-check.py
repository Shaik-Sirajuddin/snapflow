#!/usr/bin/env python3
"""Full-chain (MCP-adapter-equivalent) headless parity check: drives a real
`snapshotd` daemon instance's own SDP control socket -- the exact
`daemon.createProject` / `daemon.launch({headless})` / `project.select` /
`sap.*` calls the MCP server itself forwards -- against a `SNAPSHOT_BIN_PATH`
pointed at the real Qt/C-ABI Shotcut binary (real_ffi build), once with
headless=true and once with headless=false on the live VNC display.

This closes the "mcp -> rpc rust -> qt in memory shotcut" chain: unlike
scripts/headless-parity-check.py (which drives the Shotcut binary directly),
this script goes through the daemon's own process-manager launch path
(internal/procmgr), the same path daemon.launch's MCP tool call exercises,
so it proves the *daemon*, not just the FFI backend in isolation, respects
SNAPSHOT_HEADLESS and produces the same composited project state either way.

Usage:
  scripts/daemon-headless-parity-check.py --control-socket PATH [--display :100]
"""
from __future__ import annotations

import argparse
import base64
import io
import json
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

ARTIFACT_DIR = Path.home() / "Desktop" / "content" / "verification" / "headless-parity"
SAMPLE_FRAMES = [0, 10, 25, 40, 60, 74]


class ControlClient:
    """Newline-delimited JSON-RPC 2.0 client for the daemon's own SDP
    control socket (snapshotd/internal/sdp/protocol.go) -- different
    framing from sap-rust's Content-Length framing (see
    scripts/build-montage-frames.py's docstring for the same distinction)."""

    def __init__(self, sock_path: str):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(60.0)
        self.sock.connect(sock_path)
        self.rfile = self.sock.makefile("r", encoding="utf-8")
        self.next_id = 1

    def close(self):
        self.sock.close()

    def call(self, method: str, params: dict) -> dict:
        rid = self.next_id
        self.next_id += 1
        req = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        self.sock.sendall((json.dumps(req) + "\n").encode("utf-8"))
        while True:
            line = self.rfile.readline()
            if not line:
                raise EOFError("daemon control socket closed")
            msg = json.loads(line)
            if msg.get("id") == rid:
                if msg.get("error"):
                    raise SystemExit(f"FAIL {method}: {msg['error']}")
                return msg["result"]


def png_extrema(png_bytes: bytes):
    from PIL import Image
    return Image.open(io.BytesIO(png_bytes)).convert("RGB").getextrema()


def run_one(client: ControlClient, label: str, headless: bool, asset_path: Path, display: str) -> dict:
    name = f"qtparity-{label}-{uuid.uuid4().hex[:8]}"
    proj = client.call("daemon.createProject", {"name": name})
    project_id = proj.get("id") or proj.get("ID")
    print(f"  created project {project_id} ({name}) root={proj.get('rootDir') or proj.get('RootDir')}")

    launch = client.call("daemon.launch", {"projectId": project_id, "headless": headless})
    print(f"  daemon.launch(headless={headless}) -> {launch}")

    client.call("project.select", {"projectId": project_id})

    if not headless:
        time.sleep(2.0)
        shot_path = ARTIFACT_DIR / f"daemon_{label}_window.png"
        subprocess.run(["scrot", "-o", str(shot_path)], env={"DISPLAY": display, "PATH": "/usr/bin:/bin"}, check=True)
        extrema = png_extrema(shot_path.read_bytes())
        print(f"  screenshot -> {shot_path} extrema={extrema}")
        if all(mx == 0 for _lo, mx in extrema) or all(lo == hi for lo, hi in extrema):
            raise SystemExit(f"{label}: screenshot blank/uniform, no real window shown")

    track = client.call("edit.addTrack", {"kind": "video"})
    track_index = track["index"]
    clip = client.call("edit.appendClip", {"trackIndex": track_index, "source": {"path": str(asset_path)}})
    client.call("filter.add", {
        "clipId": clip["clipId"], "mltService": "qtblend",
        "properties": {"rect": "0=5% 5% 90% 90% 100"},
    })

    frames = {}
    for fnum in SAMPLE_FRAMES:
        resp = client.call("playback.getFrame", {"frame": fnum, "format": "png"})
        frames[fnum] = base64.b64decode(resp["data"])

    return {"project_id": project_id, "track": track, "clip": clip, "frames": frames}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--control-socket", required=True)
    ap.add_argument("--display", default=":100")
    args = ap.parse_args()

    asset_path = ARTIFACT_DIR / "assets" / "parity-test-clip.mp4"
    if not asset_path.exists():
        raise SystemExit(f"expected test asset at {asset_path} (run headless-parity-check.py first)")

    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    results = {}
    for label, headless in (("headless", True), ("non_headless", False)):
        print(f"=== {label} (headless={headless}) via daemon.launch ===")
        client = ControlClient(args.control_socket)
        try:
            results[label] = run_one(client, label, headless, asset_path, args.display)
        finally:
            client.close()
        # Give the just-launched child a moment to fully exit / release the
        # display connection before the next daemon.launch spawns another.
        time.sleep(1.0)

    print("\n=== comparing daemon-launched headless vs non-headless frames ===")
    mismatches = []
    for fnum in SAMPLE_FRAMES:
        a = results["headless"]["frames"][fnum]
        b = results["non_headless"]["frames"][fnum]
        if a == b:
            print(f"frame {fnum}: IDENTICAL ({len(a)} bytes)")
        else:
            from PIL import Image, ImageChops
            ia = Image.open(io.BytesIO(a)).convert("RGB")
            ib = Image.open(io.BytesIO(b)).convert("RGB")
            if ia.size != ib.size:
                mismatches.append(f"frame {fnum}: size differs {ia.size} vs {ib.size}")
                continue
            max_diff = max(c for band in ImageChops.difference(ia, ib).getextrema() for c in band)
            if max_diff != 0:
                mismatches.append(f"frame {fnum}: pixel max_diff={max_diff}")
            else:
                print(f"frame {fnum}: pixel-identical (max_diff=0)")

    if mismatches:
        for m in mismatches:
            print(f"MISMATCH: {m}")
        raise SystemExit(1)

    print("\nPASS: daemon.launch({headless:true}) and daemon.launch({headless:false}) "
          "against the real Qt/C-ABI Shotcut binary produced pixel-identical "
          "playback.getFrame output, and the non-headless launch showed a real "
          f"window on {args.display}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
