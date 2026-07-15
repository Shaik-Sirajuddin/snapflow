#!/usr/bin/env python3
"""One-off repro for the transitions.addCrossfade(clips 3,4) rejection seen
in TestMCPAdapter_SapCallTool_RealExport_EndToEnd. Drives the real Qt
shotcut binary directly (same SapClient wire protocol as
scripts/headless-parity-check.py) through the identical sequence, printing
edit.listClips before each crossfade call and dumping the child's stderr
(which includes the sap_ffi.cpp qWarning diagnostic on rejection)."""
from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


class SapClient:
    def __init__(self, sock_path: Path, connect_timeout: float = 30.0):
        deadline = time.monotonic() + connect_timeout
        self.sock = None
        last_err = None
        while time.monotonic() < deadline:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(30.0)
                s.connect(str(sock_path))
                self.sock = s
                break
            except OSError as e:
                last_err = e
                time.sleep(0.25)
        if self.sock is None:
            raise SystemExit(f"connect failed: {last_err}")
        self.rfile = self.sock.makefile("rb")
        self.next_id = 1

    def _write(self, value):
        body = json.dumps(value, separators=(",", ":")).encode()
        header = f"Content-Length: {len(body)}\r\n\r\n".encode()
        self.sock.sendall(header + body)

    def _read(self):
        content_length = None
        while True:
            line = self.rfile.readline()
            if not line:
                raise EOFError("peer closed while reading headers")
            trimmed = line.decode("ascii", errors="replace").rstrip("\r\n")
            if trimmed == "":
                break
            if trimmed.lower().startswith("content-length:"):
                content_length = int(trimmed.split(":", 1)[1].strip())
        buf = self.rfile.read(content_length)
        return json.loads(buf.decode())

    def call(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            msg = self._read()
            if msg.get("id") == rid:
                return msg


def req(client, method, params, label=None):
    resp = client.call(method, params)
    if resp.get("error"):
        print(f"ERROR {label or method}: {resp['error']}")
    else:
        print(f"OK {label or method}: {json.dumps(resp['result'])[:200]}")
    return resp.get("result")


def main():
    sock_path = Path("/tmp/crossfade-repro.sock")
    log_path = Path("/tmp/crossfade-repro-shotcut.log")
    if sock_path.exists():
        sock_path.unlink()
    # Match sapcall_export_realsaprust_test.go's generateTestSource exactly
    # (30fps encode, 9s -> 225 frames at this environment's 25fps project
    # profile) so this repro reproduces the identical real-media geometry
    # the failing Go test hits, not an unrelated out-of-range trim.
    source = Path("/tmp/crossfade-repro-source.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=9",
         "-f", "lavfi", "-i", "sine=frequency=440:duration=9",
         "-c:v", "libx264", "-c:a", "aac", "-shortest", "-loglevel", "error", str(source)],
        check=True, capture_output=True,
    )

    env = os.environ.copy()
    env["SNAPSHOT_HEADLESS"] = "1"
    env["SNAPSHOT_SAP_SOCKET"] = str(sock_path)
    env["SNAPSHOT_SAP_TOKEN"] = "repro"
    env["SNAPSHOT_PROJECT_ROOT"] = "/tmp/crossfade-repro-project"
    env["HOME"] = "/tmp/crossfade-repro-home"
    env.pop("DISPLAY", None)
    Path(env["SNAPSHOT_PROJECT_ROOT"]).mkdir(parents=True, exist_ok=True)
    Path(env["HOME"]).mkdir(parents=True, exist_ok=True)
    binary = REPO_ROOT / "shotcut" / "build-real-ffi" / "src" / "shotcut"
    log_f = open(log_path, "wb")
    proc = subprocess.Popen([str(binary)], env=env, stdout=log_f, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and not sock_path.exists():
        if proc.poll() is not None:
            raise SystemExit(f"shotcut exited early: {proc.returncode}")
        time.sleep(0.2)

    client = SapClient(sock_path)
    proj = "repro-project"
    req(client, "sap.hello", {"token": "repro"}, "sap.hello")
    req(client, "project.select", {"projectId": proj})
    req(client, "edit.addTrack", {"kind": "video"})
    title = req(client, "generator.createTitle", {"mode": "simple", "text": "Repro"})
    playlist_entry = req(client, "playlist.append", {"source": {"path": str(source)}})
    req(client, "edit.appendClip", {"trackIndex": 0, "source": {"playlistIndex": title["index"]}})
    for _ in range(3):
        req(client, "edit.appendClip", {"trackIndex": 0, "source": {"playlistIndex": playlist_entry["index"]}})

    seg_len = 40
    for clip_index, start in ((1, 0), (2, 90), (3, 180)):
        req(client, "edit.trimClipOut", {"trackIndex": 0, "clipIndex": clip_index, "newFrame": start + seg_len - 1, "ripple": True}, f"trimOut(clip{clip_index})")
        req(client, "edit.trimClipIn", {"trackIndex": 0, "clipIndex": clip_index, "newFrame": start, "ripple": True}, f"trimIn(clip{clip_index})")

    clips = req(client, "edit.listClips", {"trackIndex": 0}, "listClips (before crossfade 1)")
    print(json.dumps(clips, indent=2))

    req(client, "transitions.addCrossfade", {"trackIndex": 0, "betweenClips": [1, 2], "durationFrames": 15}, "crossfade(1,2)")

    clips2 = req(client, "edit.listClips", {"trackIndex": 0}, "listClips (before crossfade 2)")
    print(json.dumps(clips2, indent=2))

    req(client, "transitions.addCrossfade", {"trackIndex": 0, "betweenClips": [3, 4], "durationFrames": 15}, "crossfade(3,4)")

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

    print("\n--- shotcut log tail ---")
    print(log_path.read_text(errors="replace")[-4000:])


if __name__ == "__main__":
    main()
