#!/usr/bin/env python3
"""One-off repro for TestMCPAdapter_PhaseB_SameProjectConcurrency's
last-write-wins step: filter.add + two filter.setProperty calls, then
project.undo + project.redo, then dump project.exportProjectXml -- isolates
whether the filter ever gets attached at all vs. whether undo/redo removes
it (sap_filter_add/sap_filter_set_property push no QUndoCommand, so a real
QUndoStack undo() has nothing filter-related of theirs to undo -- the
question is what IS on the stack and whether undoing it collaterally drops
the filter)."""
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
    sock_path = Path("/tmp/filter-undo-repro.sock")
    log_path = Path("/tmp/filter-undo-repro-shotcut.log")
    if sock_path.exists():
        sock_path.unlink()
    source = Path("/tmp/filter-undo-repro-source.mp4")
    subprocess.run(
        ["ffmpeg", "-y", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=2",
         "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
         "-c:v", "libx264", "-c:a", "aac", "-shortest", "-loglevel", "error", str(source)],
        check=True, capture_output=True,
    )

    env = os.environ.copy()
    env["SNAPSHOT_HEADLESS"] = "1"
    env["SNAPSHOT_SAP_SOCKET"] = str(sock_path)
    env["SNAPSHOT_SAP_TOKEN"] = "repro"
    env["SNAPSHOT_PROJECT_ROOT"] = "/tmp/filter-undo-repro-project"
    env["HOME"] = "/tmp/filter-undo-repro-home"
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
    req(client, "edit.addTrack", {"kind": "audio"})
    req(client, "edit.addTrack", {"kind": "video"})
    appended = req(client, "playlist.append", {"source": {"path": str(source)}})
    clip = req(client, "edit.appendClip", {"trackIndex": 0, "source": {"playlistIndex": appended["index"]}})
    clip_id = clip["clipId"]
    print("clipId =", clip_id)

    state0 = req(client, "project.getState", {}, "getState (before filter.add)")

    filt = req(client, "filter.add", {"clipId": clip_id, "mltService": "brightness", "properties": {}})
    filter_index = filt["filterIndex"]

    state1 = req(client, "project.getState", {}, "getState (after filter.add)")

    req(client, "filter.setProperty", {"clipId": clip_id, "filterIndex": filter_index, "property": "level", "value": 0.25}, "setProperty(A=0.25)")
    state2 = req(client, "project.getState", {}, "getState (after setProperty A)")
    req(client, "filter.setProperty", {"clipId": clip_id, "filterIndex": filter_index, "property": "level", "value": 0.75}, "setProperty(B=0.75)")
    state3 = req(client, "project.getState", {}, "getState (after setProperty B)")

    print("undoDepth progression:", state0.get("undoDepth"), "->", state1.get("undoDepth"), "->", state2.get("undoDepth"), "->", state3.get("undoDepth"))

    project_mlt = Path(env["SNAPSHOT_PROJECT_ROOT"]) / "project.mlt"

    def dump_xml(label):
        req(client, "project.save", {}, f"project.save ({label})")
        xml = project_mlt.read_text()
        has_brightness = "brightness" in xml
        has_075 = '<property name="level">0.75</property>' in xml
        has_025 = '<property name="level">0.25</property>' in xml
        print(f"[{label}] brightness={has_brightness} level=0.75={has_075} level=0.25={has_025}")
        return xml

    dump_xml("before undo")

    req(client, "project.undo", {}, "project.undo")
    state4 = req(client, "project.getState", {}, "getState (after undo)")
    dump_xml("after undo")

    req(client, "project.redo", {}, "project.redo")
    dump_xml("after redo")

    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

    print("\n--- shotcut log tail ---")
    print(log_path.read_text(errors="replace")[-4000:])


if __name__ == "__main__":
    main()
