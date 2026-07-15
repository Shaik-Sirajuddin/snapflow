#!/usr/bin/env python3
"""Isolate whether the affine/transition.rect keyframe zoom filter actually
changes rendered pixels, independent of the crossfade scenario."""
from __future__ import annotations

import base64
import io
import json
import os
import socket
import subprocess
import time
from pathlib import Path

from PIL import Image

REPO_ROOT = Path(__file__).resolve().parent.parent


class SapClient:
    def __init__(self, sock_path, connect_timeout=30.0):
        deadline = time.monotonic() + connect_timeout
        self.sock = None
        while time.monotonic() < deadline:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(30.0)
                s.connect(str(sock_path))
                self.sock = s
                break
            except OSError:
                time.sleep(0.25)
        self.rfile = self.sock.makefile("rb")
        self.next_id = 1

    def _write(self, value):
        body = json.dumps(value, separators=(",", ":")).encode()
        self.sock.sendall(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)

    def _read(self):
        content_length = None
        while True:
            line = self.rfile.readline()
            trimmed = line.decode("ascii", errors="replace").rstrip("\r\n")
            if trimmed == "":
                break
            if trimmed.lower().startswith("content-length:"):
                content_length = int(trimmed.split(":", 1)[1].strip())
        return json.loads(self.rfile.read(content_length).decode())

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
        print(f"OK {label or method}: {json.dumps(resp['result'])[:300]}")
    return resp.get("result")


def main():
    sock_path = Path("/tmp/zoom-repro.sock")
    log_path = Path("/tmp/zoom-repro-shotcut.log")
    if sock_path.exists():
        sock_path.unlink()
    source = Path("/tmp/crossfade-repro-source.mp4")
    if not source.exists():
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
    env["SNAPSHOT_PROJECT_ROOT"] = "/tmp/zoom-repro-project"
    env["HOME"] = "/tmp/zoom-repro-home"
    env.pop("DISPLAY", None)
    Path(env["SNAPSHOT_PROJECT_ROOT"]).mkdir(parents=True, exist_ok=True)
    Path(env["HOME"]).mkdir(parents=True, exist_ok=True)
    binary = REPO_ROOT / "shotcut" / "build-real-ffi" / "src" / "shotcut"
    log_f = open(log_path, "wb")
    proc = subprocess.Popen([str(binary)], env=env, stdout=log_f, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline and not sock_path.exists():
        if proc.poll() is not None:
            raise SystemExit(f"exited early {proc.returncode}")
        time.sleep(0.2)

    client = SapClient(sock_path)
    req(client, "sap.hello", {"token": "repro"}, "hello")
    req(client, "project.select", {"projectId": "zoom-repro"})
    req(client, "edit.addTrack", {"kind": "video"})
    clip = req(client, "edit.appendClip", {"trackIndex": 0, "source": {"path": str(source)}})
    clip_id = clip["clipId"]
    zoom = req(client, "filter.add", {"clipId": clip_id, "mltService": "affine",
                                       "properties": {"transition.distort": 1}}, "filter.add")
    zoom_index = zoom["filterIndex"]
    req(client, "filter.addKeyframe", {"clipId": clip_id, "filterIndex": zoom_index,
                                        "property": "transition.rect", "position": 0,
                                        "value": "0% 0% 100% 100% 1", "interpolation": "linear"}, "kf0")
    req(client, "filter.addKeyframe", {"clipId": clip_id, "filterIndex": zoom_index,
                                        "property": "transition.rect", "position": 39,
                                        "value": "-20% -20% 140% 140% 1", "interpolation": "linear"}, "kf39")
    flist = req(client, "filter.list", {"clipId": clip_id}, "filter.list")
    print(json.dumps(flist, indent=2))

    for f in (0, 5, 20, 35, 39):
        r = req(client, "playback.getFrame", {"frame": f, "format": "png"}, f"getFrame({f})")
        raw = base64.b64decode(r["data"])
        img = Image.open(io.BytesIO(raw))
        corner = img.crop((0, 0, 80, 80))
        px = list(corner.getdata())
        avg = tuple(sum(c[i] for c in px) / len(px) for i in range(3))
        print(f"  frame {f}: size={img.size} corner-avg-rgb={avg}")

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
    print("\n--- log tail ---")
    print(log_path.read_text(errors="replace")[-3000:])


if __name__ == "__main__":
    main()
