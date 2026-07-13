#!/usr/bin/env python3
"""Final assembly for "The Film" meta-trailer: concatenates the six
segment clips (title/prompt-intro/montage/payoff/end/hello-world) with
crossfades, lays per-section audio beds + Kokoro narration on top per
memory/rui/gen/plans/trailer-narration-script.md, and muxes to the final
output. Straight ffmpeg (single filter_complex), not a second SAP/MCP
dogfooding session -- the montage segment already demonstrates the
corrected in-memory/headless-safe SAP capture pattern; this stage is pure
post-production and doesn't need to be a "live" edit.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path

ROOT = Path("/home/siraj/Desktop/content/edited_by_ai")
OUT = ROOT / "exports" / "the-film-trailer.mp4"
XFADE = 0.4
W, H, FPS = 1920, 1080, 25


def probe_duration(path: Path) -> float:
    out = subprocess.check_output([
        "ffprobe", "-v", "error", "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1", str(path),
    ])
    return float(out.strip())


VIDEO_SEGMENTS = [
    ("title", ROOT / "cards" / "title-card.mp4"),
    ("intro", ROOT / "clips" / "prompt-intro-raw.mp4"),
    ("montage", ROOT / "clips" / "editing-session-montage.mp4"),
    ("payoff", ROOT / "exports" / "gallery-output.mp4"),
    ("end", ROOT / "cards" / "end-card.mp4"),
    ("hello", ROOT / "cards" / "hello-world-card.mp4"),
]

durations = [probe_duration(p) for _, p in VIDEO_SEGMENTS]
starts = [0.0]
for d in durations[:-1]:
    starts.append(starts[-1] + d - XFADE)
total_duration = starts[-1] + durations[-1]

print("segment starts (s):")
for (name, _), s, d in zip(VIDEO_SEGMENTS, starts, durations):
    print(f"  {name:8s} start={s:7.3f} dur={d:7.3f} end={s + d:7.3f}")
print(f"total_duration={total_duration:.3f}")

title_s, intro_s, montage_s, payoff_s, end_s, hello_s = starts

# Narration lines: (file, offset_s)
NARR = [
    (ROOT / "audio/narration/narration_01.wav", title_s + 1.0),
    (ROOT / "audio/narration/narration_02.wav", intro_s + durations[1] - 2.3),
    (ROOT / "audio/narration/narration_03.wav", montage_s + 0.5),
    (ROOT / "audio/narration/narration_04.wav", payoff_s + 0.5),
    (ROOT / "audio/narration/narration_05.wav", payoff_s + 25.0),
    (ROOT / "audio/narration/narration_06.wav", end_s + 0.5),
]
narr_durations = [probe_duration(p) for p, _ in NARR]

# Bed windows: (file, start_s, length_s, gain)
BED_TITLE = (ROOT / "audio/beds/quiet-title.wav", title_s, durations[0], 0.55)
BED_INTRO = (ROOT / "audio/beds/tension-prompt.wav", intro_s, durations[1], 0.5)
BED_MONTAGE = (ROOT / "audio/beds/energetic-montage.wav", montage_s, durations[2], 0.6)
BED_END = (ROOT / "audio/beds/resolve-end.wav", end_s, total_duration - end_s, 0.5)


def fmt(x: float) -> str:
    return f"{x:.3f}"


inputs = []
for _, p in VIDEO_SEGMENTS:
    inputs += ["-i", str(p)]
for p, *_ in [BED_TITLE, BED_INTRO, BED_MONTAGE, BED_END]:
    inputs += ["-i", str(p)]
for p, _ in NARR:
    inputs += ["-i", str(p)]

n_video = len(VIDEO_SEGMENTS)
n_bed = 4
bed_base = n_video
narr_base = n_video + n_bed

filters = []

# --- video: normalize each input then chain xfade ---
for i in range(n_video):
    filters.append(
        f"[{i}:v]scale={W}:{H}:force_original_aspect_ratio=decrease,"
        f"pad={W}:{H}:(ow-iw)/2:(oh-ih)/2:color=black,fps={FPS},format=yuv420p,setsar=1[v{i}]"
    )

prev = "v0"
cum_offset = 0.0
for i in range(1, n_video):
    cum_offset = starts[i]
    out_label = f"vx{i}" if i < n_video - 1 else "vout"
    filters.append(
        f"[{prev}][v{i}]xfade=transition=fade:duration={XFADE}:offset={fmt(cum_offset)}[{out_label}]"
    )
    prev = out_label

# --- audio: beds (trim/fade/delay/duck) ---
def ms(x: float) -> int:
    return int(round(x * 1000))


def bed_filter(idx: int, path: Path, start: float, length: float, gain: float, duck_windows, label: str):
    length = max(0.1, length)
    chain = (
        f"[{idx}:a]atrim=0:{fmt(length)},"
        f"afade=t=in:st=0:d=0.3,afade=t=out:st={fmt(max(0.0, length - 0.6))}:d=0.5,"
        f"volume={gain},adelay={ms(start)}|{ms(start)}"
    )
    for w_start, w_end in duck_windows:
        chain += f",volume=0.4:enable='between(t\\,{fmt(w_start)}\\,{fmt(w_end)})'"
    chain += f"[{label}]"
    filters.append(chain)


# duck windows are absolute (global) timeline seconds -- volume's `enable`
# `between(t,...)` measures t against the filtered stream's own internal
# clock, which after adelay already matches the global timeline.
n1_end = NARR[0][1] + narr_durations[0] + 0.3
n2_end = NARR[1][1] + narr_durations[1] + 0.3
n3_end = NARR[2][1] + narr_durations[2] + 0.3
n4_end = NARR[3][1] + narr_durations[3] + 0.3
n5_end = NARR[4][1] + narr_durations[4] + 0.3
n6_end = NARR[5][1] + narr_durations[5] + 0.3

bed_filter(bed_base + 0, BED_TITLE[0], BED_TITLE[1], BED_TITLE[2], BED_TITLE[3], [(NARR[0][1], n1_end)], "bedtitle")
bed_filter(bed_base + 1, BED_INTRO[0], BED_INTRO[1], BED_INTRO[2], BED_INTRO[3], [(NARR[1][1], n2_end)], "bedintro")
bed_filter(bed_base + 2, BED_MONTAGE[0], BED_MONTAGE[1], BED_MONTAGE[2], BED_MONTAGE[3], [(NARR[2][1], n3_end)], "bedmontage")
bed_filter(bed_base + 3, BED_END[0], BED_END[1], BED_END[2], BED_END[3], [(NARR[5][1], n6_end)], "bedend")

# payoff bed = gallery-output.mp4's own embedded audio (already has
# swell-payoff.wav baked in with its own fades from the live MCP session)
filters.append(
    f"[3:a]adelay={ms(payoff_s)}|{ms(payoff_s)},"
    f"volume=0.4:enable='between(t\\,{fmt(NARR[3][1])}\\,{fmt(n4_end)})',"
    f"volume=0.4:enable='between(t\\,{fmt(NARR[4][1])}\\,{fmt(n5_end)})'[bedpayoff]"
)

# --- narration lines: delay only, reformat to common layout happens via amix ---
narr_labels = []
for i, (path, offset) in enumerate(NARR):
    label = f"nar{i}"
    filters.append(f"[{narr_base + i}:a]adelay={ms(offset)}|{ms(offset)}[{label}]")
    narr_labels.append(label)

audio_labels = ["bedtitle", "bedintro", "bedmontage", "bedpayoff", "bedend", *narr_labels]

# normalize every branch to a common format before mixing
normed = []
for lbl in audio_labels:
    nlbl = f"{lbl}n"
    filters.append(f"[{lbl}]aformat=sample_rates=48000:channel_layouts=stereo[{nlbl}]")
    normed.append(nlbl)

mix_inputs = "".join(f"[{l}]" for l in normed)
filters.append(f"{mix_inputs}amix=inputs={len(normed)}:duration=longest:normalize=0[aout]")

filter_complex = ";".join(filters)

cmd = [
    "ffmpeg", "-nostdin", "-y", "-loglevel", "error",
    *inputs,
    "-filter_complex", filter_complex,
    "-map", "[vout]", "-map", "[aout]",
    "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k",
    "-movflags", "+faststart",
    "-t", fmt(total_duration),
    str(OUT),
]

print(f"\nrunning ffmpeg ({len(filters)} filter stages, {len(inputs)//2} inputs)...")
subprocess.run(cmd, check=True)
print(f"\nDONE: {OUT}")
