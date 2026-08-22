#!/usr/bin/env sh
# Prove a built or installed `taffle` converts audio and reads the result back.
#
# Usage: scripts/smoke.sh <taffle>
#
# The argument is a path to the binary or a bare command name, so an artifact
# unpacked into a staging directory and one installed by a package manager are
# both smoked the same way.
set -eu

taffle="${1:?usage: $0 <path-to-taffle>}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The Windows runner installs the interpreter as `python`; everywhere else it is
# `python3`. Ask for what is there rather than assuming either.
py=python3
command -v "$py" >/dev/null 2>&1 || py=python

# Three seconds of a 440 Hz stereo tone at 44.1 kHz. Not 48 kHz, so the conversion
# has to resample rather than hand the audio through untouched.
"$py" - "$work/smoke.wav" <<'PY'
import math
import struct
import sys
import wave

rate, seconds, freq = 44100, 3, 440.0
with wave.open(sys.argv[1], "wb") as out:
    out.setnchannels(2)
    out.setsampwidth(2)
    out.setframerate(rate)
    frames = bytearray()
    for n in range(rate * seconds):
        sample = int(32767 * 0.5 * math.sin(2 * math.pi * freq * n / rate))
        frames += struct.pack("<hh", sample, sample)
    out.writeframes(bytes(frames))
PY

"$taffle" "$work/smoke.wav" -o "$work/smoke.taf" --no-cover
"$taffle" info "$work/smoke.taf" | tee "$work/info.txt"

# `taffle info` already exits non-zero on a file that is not the TAF its header
# describes, so reaching here proves the round trip. Naming the word as well is
# what would catch a future `info` that reports a file without validating it.
grep -q '^  valid$' "$work/info.txt"

echo "smoke ok: $taffle"
