#!/usr/bin/env bash
# Builds editing-session-montage.mp4 (trailer segment 3) from the real
# playback.getFrame captures produced by scripts/build-montage-frames.py --
# see that script's header for why this replaces screen-recording a GUI
# window: MltBackend never creates one, and the daemon's own doc comments
# confirm that's intentional ("an agent driving snapshotd has no display to
# show a GUI on in the first place"). The frames themselves are the real,
# live, in-memory timeline state at each build step, fetched over MCP/SAP.
set -euo pipefail

FRAMES_DIR="${FRAMES_DIR:-/home/siraj/Desktop/content/edited_by_ai/clips/montage-frames}"
OUT="${OUT:-/home/siraj/Desktop/content/edited_by_ai/clips/editing-session-montage.mp4}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

HOLD=1.1     # seconds each frame is held
XFADE=0.3    # crossfade duration between consecutive frames
FPS=25
W=1920
H=1080

python3 - "$FRAMES_DIR" "$WORK" <<'PY'
import json, sys
from pathlib import Path

frames_dir, work = sys.argv[1], sys.argv[2]
manifest = json.loads((Path(frames_dir) / "manifest.json").read_text())

captions = []
for step in manifest["steps"]:
    if step["step"] == "final":
        cap = "TIMELINE COMPLETE — 15 CLIPS, 2 TRACKS, 1 AUDIO BED"
    else:
        topic = step["photo"].rsplit(".", 1)[0].replace("_", " ").replace("-", " ").upper()
        cap = f"edit.appendClip + filter.add(qtblend)  |  {topic}"
        if step.get("section"):
            cap = f'title.add "{step["section"]}"  |  ' + cap
    captions.append((step["path"], cap))

(Path(work) / "captions.tsv").write_text(
    "\n".join(f"{p}\t{c}" for p, c in captions)
)
PY

i=0
SEGMENTS=()
while IFS=$'\t' read -r frame_path caption; do
  seg="$WORK/seg-$(printf '%03d' "$i").mp4"
  esc_caption=$(printf '%s' "$caption" | sed "s/:/\\\\:/g; s/'/’/g")
  ffmpeg -nostdin -y -loglevel error -loop 1 -i "$frame_path" -t "$HOLD" \
    -vf "scale=${W}:${H}:force_original_aspect_ratio=decrease,pad=${W}:${H}:(ow-iw)/2:(oh-ih)/2:color=black,format=yuv420p,\
drawbox=x=0:y=ih-90:w=iw:h=90:color=black@0.55:t=fill,\
drawtext=text='${esc_caption}':fontcolor=white:fontsize=28:x=40:y=h-58:font='DejaVu Sans Mono',\
drawtext=text='LIVE SAP / MCP SESSION':fontcolor=white@0.85:fontsize=22:x=40:y=32:font='DejaVu Sans Mono'" \
    -r "$FPS" -c:v libx264 -pix_fmt yuv420p "$seg" </dev/null
  SEGMENTS+=("$seg")
  i=$((i+1))
done < "$WORK/captions.tsv"

n=${#SEGMENTS[@]}
echo "==> ${n} caption segments built, crossfading into ${OUT}"

inputs=()
for s in "${SEGMENTS[@]}"; do
  inputs+=(-i "$s")
done

filter=""
prev="0:v"
offset=0
for ((idx=1; idx<n; idx++)); do
  cur="${idx}:v"
  out="x${idx}"
  seg_dur=$(python3 -c "print(${HOLD})")
  if [[ $idx -eq 1 ]]; then
    offset=$(python3 -c "print(${HOLD} - ${XFADE})")
  else
    offset=$(python3 -c "print(${offset} + ${HOLD} - ${XFADE})")
  fi
  if [[ $idx -eq $((n-1)) ]]; then
    out="vout"
  fi
  filter+="[${prev}][${cur}]xfade=transition=fade:duration=${XFADE}:offset=${offset}[${out}];"
  prev="$out"
done

ffmpeg -nostdin -y -loglevel error "${inputs[@]}" -filter_complex "${filter%;}" -map "[vout]" \
  -c:v libx264 -pix_fmt yuv420p -movflags +faststart "$OUT"

echo "==> DONE: $OUT"
ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1 "$OUT"
