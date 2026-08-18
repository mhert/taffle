# TAF test fixtures

Binary inputs for the `taf` crate's parser, writer and validator tests. Everything here was
generated on **2026-08-18** with the teddycloud Docker image
`ghcr.io/toniebox-reverse-engineering/teddycloud:tc_v0.6.2_ubuntu`
(digest `sha256:776b42c40728c6d3251d502031d7c1852eef578bed424a2ea662edf1da952e48`, which
reports itself as `TeddyCloud v0.6.2 (203f12d) - 2024-10-26 18:14:34 +0000 ubuntu
linux-x86_64(64)` in the OpusTags `version=` comment). Host tools: `ffmpeg`/`ffprobe` 7.x,
`docker` 29.7.2.

The format facts these fixtures back up are documented in `crates/taf/FORMAT.md`.

| File | Size | SHA-256 | Contents |
| --- | --- | --- | --- |
| `golden-sine.taf` | 114 688 B (28 blocks) | `d30231409a6acd4217f6c7c4cb7ceebd567118c2dc4c5e446c49bb75e113cad6` | Complete TAF, 10 s 440 Hz sine, 1 chapter |
| `real-header-1.bin` | 4096 B | `1016218abb467d8c3848e8dd621feeb01427157fbdf4439e3e7a2b47607cd25d` | Header block only, from a 3-chapter TAF (344 064 B) |
| `real-header-2.bin` | 4096 B | `1980a84e2d64cbb4300352bd255c3ff08a11f4a36544b7cbfc6ed92e562b92f3` | Header block only, from a 2-chapter, 5 447 680 B TAF |

## Provenance disclosure

**These fixtures are teddycloud-generated, not Toniebox-device-verified.** Device-verified TAF
files were sought under `~/toniebox/` to source `real-header-1.bin` / `real-header-2.bin`; that
directory contains only ESP32 firmware `.bin` dumps — no TAF files exist there. Both header
fixtures were therefore produced with the same encoder and the same Docker image as
`golden-sine.taf`. They exercise realistic multi-chapter headers and a realistic large
`num_bytes`, but they do not independently corroborate the format against a physical Toniebox.

`real-header-2.bin` derives from a personal audiobook. **Only the 4096-byte header block is
committed** — it holds the SHA-1 digest, the audio byte count, the audio id, two chapter block
indices and zero fill, and contains no audio whatsoever. The full TAF and the extracted MP3s
were kept out of the repository. `golden-sine.taf` is committed in full because it is
synthetic.

Re-running the commands below reproduces the *structure* but not the *bytes*: teddycloud
derives `audio_id` (and thus the Ogg serial, and thus every page CRC and the SHA-1) from
`time(NULL) - 0x50000000`, so each run yields a different file.

## Regeneration

All commands were run from a scratch directory; `$REPO` is the repository root.

```bash
# The container needs a writable config directory or settings_init() aborts. Mounting one and
# running as the host user avoids root-owned output in /tmp.
mkdir -p /tmp/tc-config
tc_encode() {  # tc_encode <output.taf> <input...>
  docker run --rm --user "$(id -u):$(id -g)" -e ASAN_OPTIONS=detect_leaks=0 \
    -v /tmp:/tmp -v /tmp/tc-config:/etc/teddycloud/config --entrypoint= \
    ghcr.io/toniebox-reverse-engineering/teddycloud:tc_v0.6.2_ubuntu \
    /usr/local/bin/teddycloud --encode "$@"
}
```

### `golden-sine.taf` — 1 chapter, complete file

```bash
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10" -ac 2 -acodec libmp3lame /tmp/sine.mp3
tc_encode /tmp/golden-sine.taf /tmp/sine.mp3
cp /tmp/golden-sine.taf "$REPO/crates/taf/tests/fixtures/golden-sine.taf"
```

Sanity check: `tail -c +4097 /tmp/golden-sine.taf | ffprobe -` reports `ogg` / `opus`,
48000 Hz, stereo, 9.66 s (a 10 s input loses its unflushed tail — see FORMAT.md,
"Tail truncation").

### `real-header-1.bin` — 3 chapters

One teddycloud invocation with three inputs; each additional input starts a chapter.

```bash
for f in 440 550 660; do
  ffmpeg -y -f lavfi -i "sine=frequency=$f:duration=10" -ac 2 -acodec libmp3lame /tmp/sine-$f.mp3
done
tc_encode /tmp/three-sines.taf /tmp/sine-440.mp3 /tmp/sine-550.mp3 /tmp/sine-660.mp3
head -c 4096 /tmp/three-sines.taf > "$REPO/crates/taf/tests/fixtures/real-header-1.bin"
```

teddycloud logged `new chapter at 0x00000000`, `0x0000001B`, `0x00000037`; the header's
`track_page_nums` is `[0, 27, 55]`. Full TAF: 344 064 B = 84 blocks, `num_bytes` 339 968.

### `real-header-2.bin` — 2 chapters, realistic size

Source: `/home/mhert/OpenAudible/books/Grimm und Möhrchen machen Pause von zu Hause (Teil 3).m4b`
(3852 s, AAC 44.1 kHz stereo). The first two 4-minute segments were extracted as MP3 and
encoded as two chapters. The explicit `-map 0:a:0 -vn -sn -dn` is required: the m4b carries a
cover-art video stream and a `bin_data` stream, and without it ffmpeg emits a 0.26 s MP3.

```bash
BOOK="/home/mhert/OpenAudible/books/Grimm und Möhrchen machen Pause von zu Hause (Teil 3).m4b"
ffmpeg -y -ss 0   -t 240 -i "$BOOK" -map 0:a:0 -vn -sn -dn -ac 2 -acodec libmp3lame /tmp/book-part1.mp3
ffmpeg -y -ss 240 -t 240 -i "$BOOK" -map 0:a:0 -vn -sn -dn -ac 2 -acodec libmp3lame /tmp/book-part2.mp3
tc_encode /tmp/book-two-chapters.taf /tmp/book-part1.mp3 /tmp/book-part2.mp3
head -c 4096 /tmp/book-two-chapters.taf > "$REPO/crates/taf/tests/fixtures/real-header-2.bin"
```

teddycloud logged `new chapter at 0x00000000` and `0x00000297`; the header's
`track_page_nums` is `[0, 663]`. Full TAF: 5 447 680 B = 1330 blocks, `num_bytes` 5 443 584,
1331 Ogg pages.

## Verified properties

Checked at generation time against the full files (not just the committed slices): 4-byte
big-endian prefix `00 00 0f fc`, protobuf fields 1..9 in order, SHA-1 over bytes 4096..EOF
matching field 1, `num_bytes` matching the audio region length, every Ogg page CRC valid,
serial == `audio_id`, contiguous page sequence numbers, one exactly-4096-byte page at every
4096 boundary from offset 8192 on, and no EOS page. Details and the exact observed values are
recorded in `crates/taf/FORMAT.md`.
