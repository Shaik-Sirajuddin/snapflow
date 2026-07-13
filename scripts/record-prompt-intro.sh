#!/usr/bin/env bash
# Launches chatbox.html full-screen (kiosk) on DISPLAY=:100 and screen-records
# the in-page typing/click/bubble animation. Mouse hidden (-draw_mouse 0)
# since the page draws its own fake cursor. Idempotent: overwrites output.
set -euo pipefail

DISPLAY_NUM="${DISPLAY_NUM:-:100}"
HTML="/home/siraj/Desktop/content/edited_by_ai/web/chatbox.html"
OUTDIR="${1:-/home/siraj/Desktop/content/edited_by_ai/clips}"
OUT="$OUTDIR/prompt-intro-raw.mp4"
DUR="${DUR:-7.5}"

mkdir -p "$OUTDIR"

# kill any stray chrome instances first so kiosk launches cleanly
pkill -f "google-chrome.*chatbox.html" 2>/dev/null || true
sleep 0.5

DISPLAY="$DISPLAY_NUM" google-chrome \
  --kiosk --no-first-run --disable-infobars --disable-session-crashed-bubble \
  --disable-features=TranslateUI --autoplay-policy=no-user-gesture-required \
  --window-position=0,0 --window-size=1920,1080 \
  "file://$HTML" >/tmp/chatbox-chrome.log 2>&1 &
CHROME_PID=$!

# give chrome time to open + paint before we start capturing
sleep 1.5

ffmpeg -y -loglevel error -f x11grab -video_size 1920x1080 -framerate 25 \
  -draw_mouse 0 -i "${DISPLAY_NUM}.0" -t "$DUR" \
  -pix_fmt yuv420p -c:v libx264 -profile:v high -an "$OUT"

kill "$CHROME_PID" 2>/dev/null || true
pkill -f "google-chrome.*chatbox.html" 2>/dev/null || true

echo "wrote $OUT"
