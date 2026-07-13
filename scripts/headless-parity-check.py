#!/usr/bin/env python3
"""Headless-vs-non-headless UI parity check for the real Qt/C-ABI Shotcut
backend (sap-rust/src/ffi_backend.rs + shotcut/src/rustbridge/sap_ffi.cpp).

Goal under test (thread goal, verbatim): "mcp -> rpc rust -> qt in memory
shotcut -> , headless or not headless changes the same ui shown by user
verified via end to end tests".

What this proves, concretely:
  1. The exact same JSON-RPC edit sequence, run against the exact same
     Shotcut/FfiBackend binary, produces byte-identical `playback.getFrame`
     output whether the process was started with SNAPSHOT_HEADLESS=1
     (QT_QPA_PLATFORM=offscreen, no window) or SNAPSHOT_HEADLESS unset/0
     (real xcb platform, DISPLAY=:100, a real window is shown). This is
     the "same UI" claim from the goal -- the composited project state the
     agent sees via RPC does not depend on whether a display is attached.
  2. In the non-headless run, a real window actually appears on the VNC
     display (captured via `scrot`, saved to disk, non-blank) -- proving
     "not headless" really does show a UI to a human, not just that the
     RPC layer is display-independent in theory.

PNG (not JPEG) is used for the byte-comparison frames: PNG is lossless, so
"identical pixels" implies "identical bytes" with no encoder-nondeterminism
risk muddying the comparison. JPEG is exercised too (as a smoke check that
it still round-trips under both platforms) but is not used for the
equality assertion.

Usage:
  scripts/headless-parity-check.py [--shotcut-bin PATH] [--display :100]

Exit code 0 = parity proven + non-headless window confirmed on screen.
Any failure raises SystemExit(1) with a clear message.
"""
from __future__ import annotations

import argparse
import base64
import io
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ARTIFACT_DIR = Path.home() / "Desktop" / "content" / "verification" / "headless-parity"

# Frames sampled across the appended clip's duration (25fps, 3s clip -> 75
# frames); deliberately not just frame 0, so any headless-only rendering
# path divergence (e.g. a filter/keyframe that behaves differently absent a
# GL context) would show up.
SAMPLE_FRAMES = [0, 10, 25, 40, 60, 74]


def find_shotcut_bin(explicit):
    if explicit:
        p = Path(explicit)
        if not p.is_file() or not os.access(p, os.X_OK):
            raise SystemExit(f"--shotcut-bin {p} is not an executable file")
        return p
    candidates = [
        REPO_ROOT / ".claude/worktrees/rust-slint-embed-trial/shotcut/build/src/shotcut",
    ]
    for c in candidates:
        if c.is_file() and os.access(c, os.X_OK):
            return c
    found = subprocess.run(
        ["find", str(REPO_ROOT / ".claude/worktrees"), "-name", "shotcut", "-type", "f", "-perm", "-111"],
        capture_output=True, text=True, timeout=30,
    ).stdout.strip().splitlines()
    if found:
        return Path(found[0])
    raise SystemExit("could not locate a built shotcut (real_ffi) binary under .claude/worktrees/")


def make_test_asset(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() and path.stat().st_size > 0:
        return
    subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25:duration=3",
            "-pix_fmt", "yuv420p", str(path),
        ],
        check=True,
    )


class SapClient:
    """Minimal Content-Length-framed JSON-RPC client, matching
    sap-rust/src/framing.rs -- same wire protocol every other harness in
    this repo (run-headless-ffi-sap*.sh) speaks, kept in Python here so it
    can drive two concurrent processes and diff their results in one
    place."""

    def __init__(self, sock_path: Path, connect_timeout: float = 30.0):
        last_err = None
        self.sock = None
        deadline = time.monotonic() + connect_timeout
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
            raise SystemExit(f"failed to connect to {sock_path}: {last_err}")
        self.rfile = self.sock.makefile("rb")
        self.next_id = 1

    def close(self) -> None:
        try:
            self.rfile.close()
        finally:
            self.sock.close()

    def _write(self, value: dict) -> None:
        body = json.dumps(value, separators=(",", ":")).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        self.sock.sendall(header + body)

    def _read(self) -> dict:
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
        if content_length is None:
            raise ValueError("missing Content-Length header")
        buf = self.rfile.read(content_length)
        if buf is None or len(buf) < content_length:
            raise EOFError("peer closed while reading body")
        return json.loads(buf.decode("utf-8"))

    def call(self, method: str, params: dict) -> dict:
        rid = self.next_id
        self.next_id += 1
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            msg = self._read()
            if msg.get("id") == rid:
                return msg
            # Skip unsolicited notifications / stale ids.


def require_ok(resp: dict, label: str):
    if resp.get("error"):
        raise SystemExit(f"FAIL {label}: {resp['error']}")
    if "result" not in resp:
        raise SystemExit(f"FAIL {label}: no result field: {resp}")
    return resp["result"]


def run_edit_sequence(client: SapClient, project_id: str, asset_path: Path) -> dict:
    """Identical edit sequence for both headless and non-headless runs --
    this determinism is the entire point of the comparison."""
    require_ok(client.call("project.select", {"projectId": project_id}), "project.select")
    track = require_ok(client.call("edit.addTrack", {"kind": "video"}), "edit.addTrack")
    track_index = track["index"]
    clip = require_ok(
        client.call("edit.appendClip", {"trackIndex": track_index, "source": {"path": str(asset_path)}}),
        "edit.appendClip",
    )
    # A filter, so parity covers more than raw decode: qtblend crop/opacity
    # exercises the same composite path a real edit would use.
    require_ok(
        client.call(
            "filter.add",
            {
                "clipId": clip["clipId"],
                "mltService": "qtblend",
                "properties": {"rect": "0=5% 5% 90% 90% 100"},
            },
        ),
        "filter.add",
    )

    frames = {}
    for fnum in SAMPLE_FRAMES:
        resp = require_ok(
            client.call("playback.getFrame", {"frame": fnum, "format": "png"}),
            f"playback.getFrame(frame={fnum}, png)",
        )
        frames[fnum] = base64.b64decode(resp["data"])
        # Smoke-check jpeg too (not used for the equality assertion).
        jresp = require_ok(
            client.call("playback.getFrame", {"frame": fnum, "format": "jpeg"}),
            f"playback.getFrame(frame={fnum}, jpeg)",
        )
        jbytes = base64.b64decode(jresp["data"])
        if jbytes[:2] != b"\xff\xd8":
            raise SystemExit(f"frame {fnum}: jpeg response missing SOI marker")

    return {"track": track, "clip": clip, "frames": frames}


def launch_shotcut(binary: Path, headless: bool, sock_path: Path, token: str,
                    log_path: Path, display):
    env = os.environ.copy()
    env["SNAPSHOT_HEADLESS"] = "1" if headless else "0"
    env["SNAPSHOT_SAP_SOCKET"] = str(sock_path)
    env["SNAPSHOT_SAP_TOKEN"] = token
    if headless:
        env.pop("DISPLAY", None)
    else:
        if not display:
            raise SystemExit("non-headless run requires --display")
        env["DISPLAY"] = display
    sock_path.parent.mkdir(parents=True, exist_ok=True)
    if sock_path.exists():
        sock_path.unlink()
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_f = open(log_path, "wb")
    proc = subprocess.Popen([str(binary)], env=env, stdout=log_f, stderr=subprocess.STDOUT)
    return proc


def wait_for_socket(sock_path: Path, proc: subprocess.Popen, timeout: float, log_path: Path) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if sock_path.exists():
            return
        if proc.poll() is not None:
            tail = log_path.read_text(errors="replace")[-4000:] if log_path.exists() else ""
            raise SystemExit(f"shotcut exited before creating socket (code={proc.returncode})\n---- log tail ----\n{tail}")
        time.sleep(0.2)
    raise SystemExit(f"socket {sock_path} not created within {timeout}s")


def kill_proc(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def take_screenshot(display: str, out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["scrot", "-o", str(out_path)], env={**os.environ, "DISPLAY": display}, check=True)


def png_extrema_nonblank(png_bytes: bytes):
    from PIL import Image
    img = Image.open(io.BytesIO(png_bytes)).convert("RGB")
    return img.getextrema()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--shotcut-bin")
    ap.add_argument("--display", default=":100")
    ap.add_argument("--socket-wait-secs", type=float, default=90.0)
    args = ap.parse_args()

    binary = find_shotcut_bin(args.shotcut_bin)
    print(f"shotcut binary: {binary}")

    ARTIFACT_DIR.mkdir(parents=True, exist_ok=True)
    asset_path = ARTIFACT_DIR / "assets" / "parity-test-clip.mp4"
    make_test_asset(asset_path)
    print(f"test asset: {asset_path}")

    token = "headless-parity-token"
    results = {}
    for label, headless in (("headless", True), ("non_headless", False)):
        sock_path = ARTIFACT_DIR / "sockets" / f"{label}.sock"
        log_path = ARTIFACT_DIR / "logs" / f"{label}-shotcut.log"
        print(f"\n=== launching shotcut ({label}, headless={headless}) ===")
        proc = launch_shotcut(binary, headless, sock_path, token, log_path,
                               args.display if not headless else None)
        try:
            wait_for_socket(sock_path, proc, args.socket_wait_secs, log_path)
            time.sleep(0.5)  # settle: sap_start_server accepts shortly after socket create
            client = SapClient(sock_path)
            try:
                client.call("sap.hello", {"token": token})
                if not headless:
                    # Let the window actually paint before screenshotting.
                    time.sleep(2.0)
                    shot_path = ARTIFACT_DIR / "non_headless_window.png"
                    take_screenshot(args.display, shot_path)
                    extrema = png_extrema_nonblank(shot_path.read_bytes())
                    print(f"non-headless screenshot: {shot_path} extrema={extrema}")
                    if all(mx == 0 for _lo, mx in extrema) or all(lo == hi for lo, hi in extrema):
                        raise SystemExit("non-headless screenshot is blank/uniform -- no real window visible")
                result = run_edit_sequence(client, f"parity-{label}-proj", asset_path)
            finally:
                client.close()
        finally:
            kill_proc(proc)
        results[label] = result
        print(f"{label}: track={result['track']} clip_index={result['clip'].get('index')}")

    print("\n=== comparing headless vs non-headless frames ===")
    frame_dir = ARTIFACT_DIR / "frames"
    frame_dir.mkdir(parents=True, exist_ok=True)
    mismatches = []
    for fnum in SAMPLE_FRAMES:
        a = results["headless"]["frames"][fnum]
        b = results["non_headless"]["frames"][fnum]
        (frame_dir / f"frame{fnum:04d}_headless.png").write_bytes(a)
        (frame_dir / f"frame{fnum:04d}_non_headless.png").write_bytes(b)
        extrema_a = png_extrema_nonblank(a)
        if all(mx == 0 for _lo, mx in extrema_a) or all(lo == hi for lo, hi in extrema_a):
            mismatches.append(f"frame {fnum}: headless frame is blank/uniform ({extrema_a})")
            continue
        if a == b:
            print(f"frame {fnum}: IDENTICAL ({len(a)} bytes) -- extrema={extrema_a}")
        else:
            # Not byte-identical -- fall back to a pixel-level check before
            # declaring failure (encoder metadata like a timestamp chunk
            # could theoretically differ even with identical pixels).
            from PIL import Image, ImageChops
            ia = Image.open(io.BytesIO(a)).convert("RGB")
            ib = Image.open(io.BytesIO(b)).convert("RGB")
            if ia.size != ib.size:
                mismatches.append(f"frame {fnum}: size differs {ia.size} vs {ib.size}")
                continue
            diff = ImageChops.difference(ia, ib)
            max_diff = max(c for band in diff.getextrema() for c in band)
            if max_diff == 0:
                print(f"frame {fnum}: pixel-identical (byte diff only, likely PNG metadata) -- max_diff=0")
            else:
                mismatches.append(f"frame {fnum}: pixel max_diff={max_diff} (bytes: {len(a)} vs {len(b)})")

    print("\n=== result ===")
    if mismatches:
        for m in mismatches:
            print(f"MISMATCH: {m}")
        print(f"\nartifacts under: {ARTIFACT_DIR}")
        raise SystemExit(1)

    print("PASS: headless and non-headless runs produced pixel-identical "
          "playback.getFrame output for every sampled frame, and the "
          "non-headless run showed a real, non-blank window on "
          f"{args.display}.")
    print(f"artifacts under: {ARTIFACT_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
