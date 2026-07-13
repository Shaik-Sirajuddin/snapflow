#!/usr/bin/env bash
# Generates test assets for the "photo gallery" live E2E scenario:
#   - 4 distinct ~8s source video clips (ffmpeg lavfi test-pattern generators)
#   - 40 distinct numbered still images (color-cycled + drawtext)
#   - 1 background audio track spanning the whole timeline
#
# Output layout under OUTDIR (default /home/siraj/Desktop/content/edited_by_ai/gallery):
#   clips/clip1.mp4 .. clip4.mp4
#   images/img001.png .. img040.png
#   audio/bgm.wav
#
# Idempotent: re-running regenerates all assets from scratch.
set -euo pipefail

OUTDIR="${1:-/home/siraj/Desktop/content/edited_by_ai/gallery}"
FPS=25
SIZE=1280x720
FONT=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf

CLIPS_DIR="$OUTDIR/clips"
IMAGES_DIR="$OUTDIR/images"
AUDIO_DIR="$OUTDIR/audio"

mkdir -p "$CLIPS_DIR" "$IMAGES_DIR" "$AUDIO_DIR"

echo "==> Generating source video clips in $CLIPS_DIR"

# name:lavfi-source pairs -- four visually distinct generators, 8s each,
# each burned with a label so clips are trivially distinguishable by eye
# and by pixel sampling (used for the "cut back to background video"
# interleave portion of the timeline).
declare -a CLIP_NAMES=(clip1 clip2 clip3 clip4)
declare -a CLIP_SOURCES=(
  "testsrc2=size=${SIZE}:rate=${FPS}:duration=8"
  "mandelbrot=size=${SIZE}:rate=${FPS}:maxiter=100"
  "gradients=size=${SIZE}:rate=${FPS}:speed=0.02:nb_colors=6"
  "rgbtestsrc=size=${SIZE}:rate=${FPS}:duration=8"
)
declare -a CLIP_LABELS=("CLIP 1 - testsrc2" "CLIP 2 - mandelbrot" "CLIP 3 - gradients" "CLIP 4 - rgbtestsrc")

for i in "${!CLIP_NAMES[@]}"; do
  name="${CLIP_NAMES[$i]}"
  src="${CLIP_SOURCES[$i]}"
  label="${CLIP_LABELS[$i]}"
  out="$CLIPS_DIR/${name}.mp4"
  echo "  - $out ($src)"
  ffmpeg -y -loglevel error -f lavfi -i "$src" -t 8 \
    -vf "drawtext=fontfile=${FONT}:text='${label}':fontcolor=white:fontsize=48:x=40:y=40:box=1:boxcolor=black@0.5:boxborderw=12" \
    -pix_fmt yuv420p -c:v libx264 -profile:v high -r "$FPS" \
    -an "$out"
done

echo "==> Generating 40 numbered gallery still images in $IMAGES_DIR"

# 40 distinct hues spread evenly around the color wheel so every image is
# trivially distinguishable by pixel sampling. Computed once in python and
# consumed as hex colors by ffmpeg's color= source.
HEX_COLORS=$(python3 - <<'PY'
import colorsys
for i in range(40):
    h = i / 40.0
    r, g, b = colorsys.hsv_to_rgb(h, 0.65, 0.95)
    print("%02x%02x%02x" % (int(r * 255), int(g * 255), int(b * 255)))
PY
)

i=1
while read -r hexcolor; do
  n=$(printf "%03d" "$i")
  out="$IMAGES_DIR/img${n}.png"
  ffmpeg -y -loglevel error -f lavfi -i "color=c=0x${hexcolor}:s=${SIZE}" -frames:v 1 \
    -vf "drawtext=fontfile=${FONT}:text='PHOTO ${i} / 40':fontcolor=white:fontsize=80:x=(w-text_w)/2:y=(h-text_h)/2:box=1:boxcolor=black@0.4:boxborderw=20" \
    "$out"
  i=$((i + 1))
done <<< "$HEX_COLORS"

echo "==> Generating background audio track in $AUDIO_DIR"

# ~70s of a gentle two-tone pad (well beyond the full timeline length so it
# can be trimmed to fit); real decodable WAV audio, not silence, so
# audio.setGain/setFadeInOut/setNormalize have real samples to act on.
ffmpeg -y -loglevel error -f lavfi \
  -i "sine=frequency=220:duration=70,volume=0.35" \
  -f lavfi -i "sine=frequency=330:duration=70,volume=0.2" \
  -filter_complex "[0:a][1:a]amix=inputs=2:duration=first[aout]" \
  -map "[aout]" -ac 2 -ar 44100 "$AUDIO_DIR/bgm.wav"

echo "==> Done."
echo "clips:  $(ls "$CLIPS_DIR" | wc -l) files in $CLIPS_DIR"
echo "images: $(ls "$IMAGES_DIR" | wc -l) files in $IMAGES_DIR"
echo "audio:  $(ls "$AUDIO_DIR" | wc -l) files in $AUDIO_DIR"
