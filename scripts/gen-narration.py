#!/usr/bin/env python3
"""Generate trailer narration WAVs via kokoro-onnx.

Reads the fixed line list below (kept in sync with
memory/rui/gen/plans/trailer-narration-script.md) and renders one WAV per
line into the project audio directory. Idempotent: re-running overwrites
the same numbered files.
"""
import os
import soundfile as sf
from kokoro_onnx import Kokoro

PROJECT = "/home/siraj/Desktop/content/edited_by_ai"
MODEL_DIR = os.path.join(PROJECT, "tts-model")
OUT_DIR = os.path.join(PROJECT, "audio", "narration")

VOICE = "af_heart"
SPEED = 0.95

LINES = [
    ("narration_01", "Every film starts with a prompt."),
    ("narration_02", "An agent is listening."),
    ("narration_03", "Watch it think. Watch it build."),
    ("narration_04", "This is what it built."),
    ("narration_05", "Real photos. Real motion. A real edit."),
    ("narration_06", "Edited by AI."),
]


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    kokoro = Kokoro(
        os.path.join(MODEL_DIR, "kokoro-v1.0.onnx"),
        os.path.join(MODEL_DIR, "voices-v1.0.bin"),
    )
    for name, text in LINES:
        samples, sr = kokoro.create(text, voice=VOICE, speed=SPEED, lang="en-us")
        out_path = os.path.join(OUT_DIR, f"{name}.wav")
        sf.write(out_path, samples, sr)
        dur = len(samples) / sr
        print(f"wrote {out_path} ({dur:.2f}s) :: {text!r}")


if __name__ == "__main__":
    main()
