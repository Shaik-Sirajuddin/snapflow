#!/usr/bin/env bash
# Renders the trailer's title/end/hello-world cards as standalone silent
# MP4 clips (1920x1080, 25fps). Cinematic dark background with a slow
# gradient drift, two-stage drawtext fade-ins. Idempotent.
set -euo pipefail

OUTDIR="${1:-/home/siraj/Desktop/content/edited_by_ai/cards}"
mkdir -p "$OUTDIR"

SIZE=1920x1080
FPS=25
SERIF=/usr/share/fonts/truetype/dejavu/DejaVuSerif-Bold.ttf
SANS=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf
# NOTE: only "THE FILM" itself uses SERIF; every other text element across
# all three cards (byline, end-card, hello-world) uses SANS for consistency.

# --- 1. Title card: "THE FILM" -> byline "Editing by AI / Edited by AI" ---
DUR=5
ffmpeg -y -loglevel error -f lavfi -i "gradients=size=${SIZE}:rate=${FPS}:speed=0.006:nb_colors=3:duration=${DUR}" \
  -vf "eq=brightness=-0.42:saturation=0.25:contrast=1.05,vignette=PI/3.2,\
drawtext=fontfile=${SERIF}:text='THE FILM':fontcolor=white:fontsize=130:\
x=(w-text_w)/2:y=(h-text_h)/2-40:borderw=2:bordercolor=black@0.6:\
alpha='if(lt(t\,0.3)\,0\,if(lt(t\,1.4)\,(t-0.3)/1.1\,1))',\
drawtext=fontfile=${SANS}:text='EDITING BY AI  |  EDITED BY AI':fontcolor=0xE8C468:fontsize=34:\
x=(w-text_w)/2:y=(h-text_h)/2+90:borderw=1:bordercolor=black@0.6:\
alpha='if(lt(t\,2.0)\,0\,if(lt(t\,3.1)\,(t-2.0)/1.1\,1))'" \
  -pix_fmt yuv420p -c:v libx264 -profile:v high -r "$FPS" -an "$OUTDIR/title-card.mp4"
echo "wrote $OUTDIR/title-card.mp4"

# --- 2. End card: sign-off wordmark ---
DUR=3.5
ffmpeg -y -loglevel error -f lavfi -i "gradients=size=${SIZE}:rate=${FPS}:speed=0.006:nb_colors=3:duration=${DUR}" \
  -vf "eq=brightness=-0.42:saturation=0.25:contrast=1.05,vignette=PI/3.2,\
drawtext=fontfile=${SERIF}:text='SAP-RUST':fontcolor=white:fontsize=90:\
x=(w-text_w)/2:y=(h-text_h)/2-30:borderw=2:bordercolor=black@0.6:\
alpha='if(lt(t\,0.2)\,0\,if(lt(t\,1.1)\,(t-0.2)/0.9\,1))',\
drawtext=fontfile=${SANS}:text='an autonomous video editor':fontcolor=0xB0B0B0:fontsize=30:\
x=(w-text_w)/2:y=(h-text_h)/2+70:borderw=1:bordercolor=black@0.6:\
alpha='if(lt(t\,1.4)\,0\,if(lt(t\,2.3)\,(t-1.4)/0.9\,1))'" \
  -pix_fmt yuv420p -c:v libx264 -profile:v high -r "$FPS" -an "$OUTDIR/end-card.mp4"
echo "wrote $OUTDIR/end-card.mp4"

# --- 3. Hello World card: same gradient/grade/vignette background and the
# same two-stage fade curve shape as cards 1-2 (see "Card animation
# styling plan" in memory/rui/gen/plans/ai-film-meta-trailer.md), so it
# visually rhymes with the title/end cards rather than standing apart on a
# flat background. The mint-green text color and blinking cursor are the
# one deliberate exception -- a bookend motif matching the terminal/
# chatbox look of the prompt-intro segment this card closes out.
DUR=3
ffmpeg -y -loglevel error -f lavfi -i "gradients=size=${SIZE}:rate=${FPS}:speed=0.006:nb_colors=3:duration=${DUR}" \
  -vf "eq=brightness=-0.42:saturation=0.25:contrast=1.05,vignette=PI/3.2,\
drawtext=fontfile=${SANS}:text='Hello World':fontcolor=0x6EE7B7:fontsize=64:\
x=(w-text_w)/2:y=(h-text_h)/2:borderw=1:bordercolor=black@0.6:\
alpha='if(lt(t\,0.18)\,0\,if(lt(t\,0.85)\,(t-0.18)/0.67\,1))',\
drawtext=fontfile=${SANS}:text='_':fontcolor=0x6EE7B7:fontsize=64:\
x=(w-text_w)/2+300:y=(h-text_h)/2:\
alpha='if(lt(t\,0.18)\,0\,lt(mod(t\,0.5)\,0.25))'" \
  -pix_fmt yuv420p -c:v libx264 -profile:v high -r "$FPS" -an "$OUTDIR/hello-world-card.mp4"
echo "wrote $OUTDIR/hello-world-card.mp4"
