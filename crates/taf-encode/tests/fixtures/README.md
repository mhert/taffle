# taf-encode test fixtures

Encoded inputs for the decode tests. Everything here was generated on **2026-08-19** with
**ffmpeg n9.0.1** (Arch Linux build, `--enable-libmp3lame`), from nothing but ffmpeg's own
`lavfi` sources — no third-party audio, no personal files.

| File | Size | SHA-256 | Contents |
| --- | --- | --- | --- |
| `cover.png` | 120 B | `ab58d03dfa052967fddbe7d794188e1744b773ed8137b759f4a14a5b90853502` | 32×32 solid orange, the cover embedded in the two below |
| `tiny.m4b` | 83 748 B | `72b1edd275301c2a7276749a0b0ddfb1d4eef4a3cb67623dd3e4da70ca43cb0f` | 10 s AAC in MP4, 2 chapters, PNG cover |
| `tiny.mp3` | 80 624 B | `c9342d2def937fc9be9761204f0fc7f9fe4de86db86abc65867462d838c7a872` | 10 s MPEG-1 Layer III, ID3v2.3 `APIC` cover |
| `video-first.mp4` | 5 952 B | `ef4e160bcc3cc01a2e6477d3148f22476292995a8b324cc4cb0f956ae5b9b9e2` | Video track, then 1 s of mono AAC |
| `no-audio.mp4` | 1 024 B | `a192e82fbb172343ad6bd8340a458fc38fbc5bba6fe95c9c757010c01ffdf764` | One video track and nothing else |
| `mp3-in-mp4.mp4` | 5 066 B | `a86ecb2c2ded5c64390133cc6cc1810c0dec45a600bd80a3cc9db0e6c7beae03` | 1 s of MPEG audio in an MP4 |

The constants the tests assert against are in `crates/taf-encode/tests/fixtures.rs`; what each file
is *for* is in the doc comment there.

## The tone

`tiny.m4b` and `tiny.mp3` carry the same signal: 10 seconds of a 440 Hz sine at 44 100 Hz, stereo,
441 000 frames. ffmpeg's `sine` source has a fixed amplitude of 1/8 full scale and `-ac 2` upmixes
its one channel with the standard centre-to-front coefficient of 1/√2, so `volume=4` authors the
tone at 1/8 × 4 × 1/√2 = 0.354 full scale. Measured off the PCM ffmpeg produces from the same
filter chain, that is a **peak of 11 582** — the number `ENCODED_PEAK` states.

The rate is deliberately not the 48 kHz a TAF ends up at, and it is not the rate of the WAV
fixture's *duration* either, so nothing downstream can assume an input already arrives at the
output rate.

## Regeneration

Run from any scratch directory. `$FIXTURES` is this directory. Re-running every command below on
the same ffmpeg build reproduces all six files byte for byte — that was checked against the
SHA-256 sums above, and `-fflags +bitexact` is what makes it hold.

### `chapters.txt` — the chapter marks `tiny.m4b` is authored with

Not committed; it exists only to be fed to ffmpeg.

```bash
cat > chapters.txt <<'EOF'
;FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=5000
title=Anfang
[CHAPTER]
TIMEBASE=1/1000
START=5000
END=10000
title=Möhrchen macht Pause
EOF
```

The second title carries an umlaut on purpose: `chpl` states a title's length in bytes, so a
title whose byte count is not its character count is the one that catches a reader confusing the
two.

### `cover.png`

```bash
ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "color=c=#ff8000:s=32x32:d=1" \
  -frames:v 1 -fflags +bitexact -flags:v +bitexact "$FIXTURES/cover.png"
```

### `tiny.m4b`

```bash
ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" \
  -i "$FIXTURES/cover.png" -f ffmetadata -i chapters.txt \
  -map 0:a -map 1:v -map_chapters 2 \
  -af volume=4 -ac 2 -c:a aac -b:a 64k \
  -c:v copy -disposition:v attached_pic \
  -fflags +bitexact -f ipod "$FIXTURES/tiny.m4b"
```

`-f ipod` is the m4b muxer. `-fflags +bitexact` keeps ffmpeg's version out of the file, which is
what makes a regeneration byte-comparable to the committed one.

### `tiny.mp3`

```bash
ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" \
  -i "$FIXTURES/cover.png" \
  -map 0:a -map 1:v \
  -af volume=4 -ac 2 -c:a libmp3lame -b:a 64k \
  -c:v copy -disposition:v attached_pic -id3v2_version 3 \
  -metadata:s:v title="Cover" -metadata:s:v comment="Cover (front)" \
  -fflags +bitexact "$FIXTURES/tiny.mp3"
```

### `video-first.mp4`, `no-audio.mp4` and `mp3-in-mp4.mp4`

None of them carries chapters or a cover; they exist for what a container states about its tracks
and nothing else.

```bash
ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "color=c=black:s=32x32:r=1:d=1" \
  -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" \
  -map 0:v -map 1:a -frames:v 1 -c:v mjpeg -af volume=4 -ac 1 -c:a aac -b:a 32k \
  -fflags +bitexact -f mp4 "$FIXTURES/video-first.mp4"

ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "color=c=black:s=32x32:r=1:d=1" \
  -map 0:v -frames:v 1 -c:v mjpeg \
  -fflags +bitexact -f mp4 "$FIXTURES/no-audio.mp4"

ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" \
  -af volume=4 -ac 1 -c:a libmp3lame -b:a 32k \
  -fflags +bitexact -f mp4 "$FIXTURES/mp3-in-mp4.mp4"
```

### The authored peak

How `ENCODED_PEAK` was measured, straight off the tone before any encoder saw it:

```bash
ffmpeg -hide_banner -y -loglevel error \
  -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" \
  -af volume=4 -ac 2 -c:a pcm_s16le -f s16le authored.raw
python3 -c "
import struct
raw = open('authored.raw','rb').read()
print(max(abs(v) for v in struct.unpack(f'<{len(raw)//2}h', raw)))"
```

## Verified properties

Checked against the generated files at the time they were committed.

### `tiny.m4b`

- Layout `ftyp / free / mdat / moov`, so the demuxer and the chapter reader both have to seek past
  80 KB of audio to reach the header — the opposite order from the audiobooks on this machine,
  which put `moov` first.
- `moov/udta/chpl` is 62 bytes: version 1, count 2, entries `{0, 6, "Anfang"}` and
  `{50 000 000, 21, "Möhrchen macht Pause"}`. The second title is 20 characters in 21 bytes.
- `moov/udta/meta/ilst/covr` holds data type 14 (PNG) and exactly the 120 bytes of `cover.png`.
- ffmpeg also writes a QuickTime chapter *track* (a second `trak`, referenced from the audio
  track's `tref/chap`) stating the same two marks at frames 0 and 220 500. Nothing reads it — see
  the module comment in `src/decode/mp4_chapters.rs`.
- 432 AAC packets of 1024 frames = 442 368 frames decoded, against 441 000 authored: the encoder's
  1024 priming frames plus 344 of padding. symphonia's MP4 demuxer implements no gapless trimming,
  so all of them come out as audio.
- Decoded peak 12 260, which is 5.9 % above the authored 11 582.

### `tiny.mp3`

- ID3v2.3 tag first, one `APIC` frame: MIME `image/png`, picture type 3 (front cover), description
  `Cover`, and exactly the 120 bytes of `cover.png`.
- LAME's `Info` header states 1105 frames of encoder delay and 263 of padding; 384 packets of 1152
  frames = 442 368, less 1105 and 263, is exactly the 441 000 authored. With gapless playback
  enabled — which is what `open_source` asks for — that is what symphonia hands out.
- Decoded peak 11 209, which is 3.2 % below the authored 11 582.

### `video-first.mp4`

- Two tracks: MJPEG first, AAC second. symphonia states the first as codec type 0, which is how
  a decode that took a container's first track finds no audio in this file.
- The audio is one second of mono at 44 100 Hz, so a source that decoded the wrong track — or read
  the channel count off the wrong one — reports the wrong shape.

### `no-audio.mp4`

- One MJPEG track. symphonia opens the container and states no track any decoder can take, which
  is the only way `DecodeError::NoAudioTrack` can be reached with a real file.

### `mp3-in-mp4.mp4`

- One track of MPEG audio, which an MP4 describes with an object type and a sample rate and no
  channel count — and symphonia's MPEG decoder, unlike its AAC one, does not work one out from the
  codec's configuration either. Nothing states the shape of this stream until a frame of it has
  been decoded, which is the case `open_source` refuses.
