#!/usr/bin/env bash
# Synthesizes 5 procedurally-generated background beds for the trailer's
# per-section audio (see "Narration" section of
# memory/rui/gen/plans/ai-film-meta-trailer.md and the bed column in
# trailer-narration-script.md). Same lavfi-synthesis technique already
# validated for the gallery precedent's bgm.wav (scripts/gen-gallery-
# assets.sh) -- layered sine tones, no external audio downloads needed.
# Idempotent: overwrites all 5 files.
set -euo pipefail

OUTDIR="${1:-/home/siraj/Desktop/content/edited_by_ai/audio/beds}"
mkdir -p "$OUTDIR"
SR=44100

# 1. quiet-title (title card, ~6s): two soft low tones, gentle fade both ends.
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=110:duration=6:sample_rate=$SR,volume=0.16" \
  -f lavfi -i "sine=frequency=165:duration=6:sample_rate=$SR,volume=0.10" \
  -filter_complex "[0:a][1:a]amix=inputs=2:duration=first[m];[m]afade=t=in:d=1.5,afade=t=out:st=4.5:d=1.5[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/quiet-title.wav"

# 2. tension-prompt (prompt intro, ~8s): tremolo'd mid tone + faint pink hiss.
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=220:duration=8:sample_rate=$SR,tremolo=f=3.2:d=0.35,volume=0.14" \
  -f lavfi -i "anoisesrc=color=pink:duration=8:sample_rate=$SR,volume=0.03" \
  -filter_complex "[0:a][1:a]amix=inputs=2:duration=first[m];[m]afade=t=in:d=1:afade=t=out:st=7:d=1[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/tension-prompt.wav" 2>/tmp/bed2.err || \
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=220:duration=8:sample_rate=$SR,tremolo=f=3.2:d=0.35,volume=0.14" \
  -f lavfi -i "anoisesrc=color=pink:duration=8:sample_rate=$SR,volume=0.03" \
  -filter_complex "[0:a][1:a]amix=inputs=2:duration=first[m];[m]afade=t=in:d=1,afade=t=out:st=7:d=1[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/tension-prompt.wav"

# 3. energetic-montage (editing montage, ~16s): three-tone mix, driving pulse.
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=330:duration=16:sample_rate=$SR,volume=0.13" \
  -f lavfi -i "sine=frequency=440:duration=16:sample_rate=$SR,volume=0.10" \
  -f lavfi -i "sine=frequency=550:duration=16:sample_rate=$SR,volume=0.07" \
  -filter_complex "[0:a][1:a][2:a]amix=inputs=3:duration=first[m];[m]tremolo=f=4.5:d=0.4,afade=t=in:d=0.8,afade=t=out:st=15:d=1[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/energetic-montage.wav"

# 4. swell-payoff (real gallery payoff, ~62s): builds from quiet to fuller
#    over the first ~20s, sustains, gentle swell again near the end.
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=196:duration=62:sample_rate=$SR,volume=0.10" \
  -f lavfi -i "sine=frequency=294:duration=62:sample_rate=$SR,volume=0.08" \
  -f lavfi -i "sine=frequency=392:duration=62:sample_rate=$SR,volume=0.05" \
  -filter_complex "[0:a][1:a][2:a]amix=inputs=3:duration=first[m];[m]afade=t=in:d=8,afade=t=out:st=58:d=4[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/swell-payoff.wav"

# 5. resolve-end (end cards, ~7s): warm descending tone, long fade out.
ffmpeg -y -loglevel error \
  -f lavfi -i "sine=frequency=175:duration=7:sample_rate=$SR,volume=0.14" \
  -f lavfi -i "sine=frequency=131:duration=7:sample_rate=$SR,volume=0.09" \
  -filter_complex "[0:a][1:a]amix=inputs=2:duration=first[m];[m]afade=t=in:d=1,afade=t=out:st=3.5:d=3.5[aout]" \
  -map "[aout]" -ac 2 -ar "$SR" "$OUTDIR/resolve-end.wav"

echo "wrote 5 beds to $OUTDIR:"
ls -la "$OUTDIR"
