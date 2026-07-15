#!/usr/bin/env python3
"""Deterministic, non-LLM SAP driver for the trailer's "editing montage"
segment (segment 3 of memory/rui/gen/plans/ai-film-meta-trailer.md).

Corrected architecture note (2026-07-12): the currently-deployed backend
(snapshotd -> sap-rust/MltBackend) is a pure-Rust process with no Qt/window
ever created -- daemon.launch's `headless` flag is a documented no-op for
this path (snapshotd/internal/daemon/daemon.go's LaunchParams doc: "an
agent driving snapshotd has no display to show a GUI on in the first
place"). The project's actual guarantee -- "in headless or not, a snapshot
process should still be able to view/get its change" -- is met by
`playback.getFrame` (sap-rust/src/server.rs, backed by real `melt` renders
in mlt_backend.rs), not by screen-recording a GUI window. This script
builds a gallery timeline step by step over the daemon's raw SDP control
socket (newline-delimited JSON-RPC 2.0, snapshotd/internal/sdp/protocol.go)
and calls playback.getFrame after every meaningful mutation, so the
resulting frame sequence *is* the real, live, in-memory edit state at each
step -- no LLM driving cost, no GUI dependency.

Usage:
  python3 scripts/build-montage-frames.py \
      --control-socket ~/.snapshotd-vnc100/control.sock \
      --out-dir /home/siraj/Desktop/content/edited_by_ai/clips/montage-frames \
      --project-name the-film-montage-capture
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import sys
from pathlib import Path

PHOTOS_DIR = Path("/home/siraj/Desktop/content/edited_by_ai/photos/processed")
ICONS_DIR = Path("/home/siraj/Desktop/content/edited_by_ai/icons")
AUDIO_BED = Path("/home/siraj/Desktop/content/edited_by_ai/audio/beds/energetic-montage.wav")

# (image filename, icon filename, section label or None) -- mirrors the
# ordering used for the real gallery-output.mp4 payoff build so the montage
# visually foreshadows the payoff instead of showing unrelated content.
CLIPS = [
    ("ai-infra-datacenter_1.jpg", "server.png", "AI INFRASTRUCTURE"),
    ("ai-infra-datacenter_2.jpg", "server.png", None),
    ("ai-infra-gpu_1.jpg", "chip.png", None),
    ("ai-infra-gpu_2.jpg", "chip.png", None),
    ("ai-infra-chips_1.jpg", "chip.png", None),
    ("human-ai-medicine_1.jpg", "stethoscope.png", "HUMANS + AI AT WORK"),
    ("human-ai-education_1.jpg", "graduation-cap.png", None),
    ("human-ai-factory_1.jpg", "factory.png", None),
    ("human-ai-design_1.jpg", "palette.png", None),
    ("human-ai-elderly_1.jpg", "heart.png", None),
    ("human-ai-science_1.jpg", "leaf.png", None),
    ("human-ai-farming_1.jpg", "leaf.png", None),
    ("human-ai-music_1.jpg", "leaf.png", None),
    ("ai-infra-chips_2.jpg", "chip.png", None),
    ("ai-infra-cables_1.jpg", "server.png", None),
]

FPS = 25
CLIP_FRAMES = 100  # 4s @ 25fps, matches the real gallery build

# Pan/zoom variants cycled per clip -- percentage rect + opacity, the exact
# syntax qtblend-as-filter actually requires (confirmed against the real
# gallery-output.mp4 project's project.mlt; plain pixel values with no
# opacity component render blank/transparent, which is what the first
# attempt at this script produced -- all-identical uniform-gray frames).
VARIANTS = [
    ("0% 0% 100% 100% 100", "-6% -6% 112% 112% 100"),   # zoom-in
    ("-8% -4% 108% 108% 100", "0% -4% 108% 108% 100"),  # pan-left
    ("-6% -6% 112% 112% 100", "0% 0% 100% 100% 100"),   # zoom-out
    ("0% -4% 108% 108% 100", "-8% -4% 108% 108% 100"),  # pan-right
]


class SdpClient:
    """Newline-delimited JSON-RPC 2.0 client for snapshotd's SDP control
    socket -- see snapshotd/internal/sdp/protocol.go's doc comment. This is
    a *different* wire framing than sap-rust's own Content-Length framing;
    the daemon speaks newline-delimited JSON on its control socket and
    translates internally."""

    def __init__(self, path: str, timeout: float = 30.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.rfile = self.sock.makefile("rb")
        self._id = 0

    def call(self, method: str, params: dict | None = None):
        self._id += 1
        req = {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}}
        line = json.dumps(req, separators=(",", ":")) + "\n"
        self.sock.sendall(line.encode("utf-8"))
        while True:
            raw = self.rfile.readline()
            if not raw:
                raise EOFError(f"daemon closed connection while waiting for {method}")
            msg = json.loads(raw)
            if msg.get("id") is None:
                print(f"  [notify] {msg.get('method')}: {json.dumps(msg.get('params'))[:160]}", file=sys.stderr)
                continue
            if msg["id"] != self._id:
                continue
            if msg.get("error"):
                raise RuntimeError(f"{method} failed: {msg['error']}")
            return msg.get("result")

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def save_frame(client: SdpClient, out_dir: Path, seq: int, label: str, frame_number: int) -> Path:
    result = client.call("playback.getFrame", {"frame": frame_number, "format": "jpeg"})
    data = base64.b64decode(result["data"])
    if data[:2] != b"\xff\xd8":
        raise RuntimeError(f"getFrame({frame_number}) for step {seq} did not return a JPEG")
    path = out_dir / f"{seq:03d}-{label}.jpg"
    path.write_bytes(data)
    print(f"  captured frame {frame_number} -> {path.name} ({len(data)} bytes)")
    return path


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--control-socket", default=os.path.expanduser("~/.snapshotd-vnc100/control.sock"))
    ap.add_argument("--out-dir", default="/home/siraj/Desktop/content/edited_by_ai/clips/montage-frames")
    ap.add_argument("--project-name", default="the-film-montage-capture")
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    for old in out_dir.glob("*.jpg"):
        old.unlink()

    client = SdpClient(args.control_socket)
    manifest = []
    try:
        proj = client.call("daemon.createProject", {"name": args.project_name})
        project_id = proj["ID"] if "ID" in proj else proj["id"]
        print(f"created project {project_id} ({proj.get('RootDir') or proj.get('rootDir')})")

        client.call("daemon.launch", {"projectId": project_id, "headless": True})
        client.call("project.select", {"projectId": project_id})

        images_track = client.call("edit.addTrack", {"kind": "video"})["index"]
        overlays_track = client.call("edit.addTrack", {"kind": "video"})["index"]
        bed_track = client.call("edit.addTrack", {"kind": "audio"})["index"]
        print(f"tracks: images={images_track} overlays={overlays_track} bed={bed_track}")

        seq = 0
        cursor = 0
        for i, (photo, icon, section) in enumerate(CLIPS):
            photo_path = str(PHOTOS_DIR / photo)
            icon_path = str(ICONS_DIR / icon)

            clip = client.call("edit.appendClip", {"trackIndex": images_track, "source": {"path": photo_path}})
            start_rect, end_rect = VARIANTS[i % len(VARIANTS)]
            client.call("filter.add", {
                "clipId": clip["clipId"],
                "mltService": "qtblend",
                "properties": {
                    "rect": f"0={start_rect};{CLIP_FRAMES - 1}={end_rect}",
                    "background": "color:#00000000",
                },
            })

            overlay_clip = client.call("edit.appendClip", {"trackIndex": overlays_track, "source": {"path": icon_path}})
            client.call("filter.add", {
                "clipId": overlay_clip["clipId"],
                "mltService": "qtblend",
                "properties": {"rect": "40 40 140 140 100", "background": "color:#00000000"},
            })

            if section:
                client.call("filter.add", {
                    "clipId": clip["clipId"],
                    "mltService": "dynamictext",
                    "properties": {
                        "argument": section,
                        "geometry": "0 850 1920 100",
                        "family": "Sans",
                        "size": 64,
                        "weight": 700,
                        "fgcolour": "#FFFFFFFF",
                        "olcolour": "#AA000000",
                        "outline": 2,
                        "halign": "center",
                        "valign": "middle",
                        "in": 0,
                        "out": 49,
                    },
                })

            seq += 1
            mid_frame = cursor + CLIP_FRAMES // 2
            label = f"append-{photo.replace('.jpg', '')}"
            path = save_frame(client, out_dir, seq, label, mid_frame)
            manifest.append({"seq": seq, "step": "appendClip+filters", "photo": photo, "section": section, "frame": mid_frame, "path": str(path)})
            cursor += CLIP_FRAMES

        bed_clip = client.call("edit.appendClip", {"trackIndex": bed_track, "source": {"path": str(AUDIO_BED)}})
        try:
            client.call("audio.setFadeInOut", {"clipId": bed_clip["clipId"], "fadeInFrames": 25, "fadeOutFrames": 25})
        except RuntimeError as e:
            print(f"  [note] audio.setFadeInOut skipped: {e}", file=sys.stderr)

        seq += 1
        final_frame = max(0, cursor - CLIP_FRAMES // 2)
        path = save_frame(client, out_dir, seq, "final-timeline", final_frame)
        manifest.append({"seq": seq, "step": "final", "frame": final_frame, "path": str(path)})

        manifest_path = out_dir / "manifest.json"
        manifest_path.write_text(json.dumps({
            "projectId": project_id,
            "imagesTrack": images_track,
            "overlaysTrack": overlays_track,
            "bedTrack": bed_track,
            "totalFrames": cursor,
            "steps": manifest,
        }, indent=2))
        print(f"\nDONE: {seq} frames captured, manifest -> {manifest_path}")
        return 0
    finally:
        client.close()


if __name__ == "__main__":
    raise SystemExit(main())
